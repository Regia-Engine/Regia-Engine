// SPDX-License-Identifier: MPL-2.0
//! Seeded property tests for the scene crate (ADR-876). Each test drives a deterministic
//! `FuzzRng` stream (`regia_test_utils::fuzz`) through many random cases and checks an
//! invariant against an independent, deliberately naive reference:
//!
//! - incremental transform/visibility propagation equals a from-scratch recompute after every
//!   random hierarchy edit (spawn, reparent, keep-world reparent, detach, edit, despawn);
//!   `PreviousGlobalTransform` always holds the previous run's value; keep-world edits keep the
//!   world pose; the cycle guard keeps the hierarchy a forest;
//! - random scene files survive text, postcard and spawn → snapshot → spawn round trips;
//! - `SceneFile::validate`'s linear cycle check agrees with a naive walk on random parent maps.
//!
//! Seeds are fixed, so a failure reproduces exactly; the failing seed and step are in the message.

use std::collections::HashMap;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::{Affine3A, Quat, Vec3};
use regia_ecs::ecs::world::CommandQueue;
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::components::{EntityGuid, InheritedVisibility, Name, Tags, Visibility};
use regia_scene::format::{SceneEntity, SceneFile};
use regia_scene::hierarchy::{HierarchyCommands, would_create_cycle};
use regia_scene::propagate::propagate_transforms;
use regia_scene::spawn::{SnapshotFilter, SpawnOptions, snapshot_scene, spawn_scene};
use regia_scene::transform::{GlobalTransform, PreviousGlobalTransform, Transform};
use regia_test_utils::fuzz::FuzzRng;

/// A float in `lo..hi` from 24 random bits.
fn float(rng: &mut FuzzRng, lo: f32, hi: f32) -> f32 {
    let unit = (rng.next_u64() >> 40) as f32 / (1u64 << 24) as f32;
    lo + (hi - lo) * unit
}

/// A random TRS with a uniform scale (so keep-world reparenting is exactly representable).
fn random_transform(rng: &mut FuzzRng) -> Transform {
    Transform {
        translation: Vec3::new(
            float(rng, -10.0, 10.0),
            float(rng, -10.0, 10.0),
            float(rng, -10.0, 10.0),
        ),
        rotation: Quat::from_rotation_y(float(rng, -3.1, 3.1))
            * Quat::from_rotation_x(float(rng, -1.5, 1.5))
            * Quat::from_rotation_z(float(rng, -3.1, 3.1)),
        scale: Vec3::splat(float(rng, 0.5, 2.0)),
    }
}

fn random_visibility(rng: &mut FuzzRng) -> Visibility {
    match rng.below(3) {
        0 => Visibility::Inherited,
        1 => Visibility::Visible,
        _ => Visibility::Hidden,
    }
}

fn apply_commands(world: &mut World, edit: impl FnOnce(&mut Commands)) {
    let mut queue = CommandQueue::default();
    {
        let mut commands = Commands::new(&mut queue, world);
        edit(&mut commands);
    }
    queue.apply(world);
}

/// Part of the transform hierarchy: passes a frame to its children.
fn in_hierarchy(world: &World, entity: Entity) -> bool {
    world.get::<Transform>(entity).is_some() || world.get::<ChildOf>(entity).is_some()
}

/// Root-first ancestor chain of `entity` (inclusive).
fn chain(world: &World, entity: Entity) -> Vec<Entity> {
    let mut chain = vec![entity];
    let mut cursor = entity;
    while let Some(link) = world.get::<ChildOf>(cursor) {
        cursor = link.parent();
        assert!(!chain.contains(&cursor), "the hierarchy must stay a forest");
        chain.push(cursor);
    }
    chain.reverse();
    chain
}

/// The naive world transform: the product of every `Transform` from the top of the chain down.
fn reference_global(world: &World, entity: Entity) -> Affine3A {
    chain(world, entity)
        .into_iter()
        .filter_map(|e| world.get::<Transform>(e))
        .fold(Affine3A::IDENTITY, |frame, local| frame * local.compute_affine())
}

/// The naive resolved visibility: walk down the chain applying each `Visibility`.
fn reference_visible(world: &World, entity: Entity) -> bool {
    chain(world, entity)
        .into_iter()
        .fold(true, |visible, e| match world.get::<Visibility>(e) {
            Some(Visibility::Visible) => true,
            Some(Visibility::Hidden) => false,
            _ => visible,
        })
}

/// Equal within `relative` of the larger element magnitude (deep chains of scales grow both the
/// matrix and the translation).
fn close(a: Affine3A, b: Affine3A, relative: f32) -> bool {
    let largest = |m: Affine3A| {
        [
            m.matrix3.x_axis,
            m.matrix3.y_axis,
            m.matrix3.z_axis,
            m.translation,
        ]
        .iter()
        .map(|column| column.abs().max_element())
        .fold(0.0f32, f32::max)
    };
    a.abs_diff_eq(b, relative * (1.0 + largest(a).max(largest(b))))
}

/// Incremental propagation equals a from-scratch recompute after every random edit, the previous
/// frame is always the previous run's value, and keep-world edits keep the pose.
#[test]
fn propagation_matches_a_from_scratch_recompute() {
    for seed in 0..24u64 {
        let mut rng = FuzzRng::new(0x5CE1_0000 + seed);
        let mut world = World::new();
        let mut live: Vec<Entity> = Vec::new();
        let mut before: HashMap<Entity, Affine3A> = HashMap::new();

        for step in 0..160 {
            let context = format!("seed {seed} step {step}");
            let mut keep_world: Option<(Entity, Affine3A)> = None;
            let op = if live.len() < 4 { 0 } else { rng.below(9) };
            match op {
                // Spawn: a transform entity, a grouping node, or a visibility-only entity,
                // optionally under an existing entity.
                0 | 1 => {
                    let mut entity = world.spawn_empty();
                    match rng.below(4) {
                        0 => {
                            entity.insert(random_transform(&mut rng));
                        }
                        1 => {
                            entity.insert((random_transform(&mut rng), random_visibility(&mut rng)));
                        }
                        2 => {
                            entity.insert(random_visibility(&mut rng));
                        }
                        _ => {}
                    }
                    let id = entity.id();
                    if !live.is_empty() && rng.chance(2) {
                        let parent = live[rng.below(live.len())];
                        world.entity_mut(id).insert(ChildOf(parent));
                    }
                    live.push(id);
                }
                // Reparent (plain or keep-world); cycles must be refused.
                2 | 3 => {
                    let child = live[rng.below(live.len())];
                    let parent = live[rng.below(live.len())];
                    let refuse = would_create_cycle(&world, child, parent);
                    let old_parent = world.get::<ChildOf>(child).map(|p| p.parent());
                    if op == 3 {
                        // Only an entity inside the hierarchy has a world pose to keep.
                        if in_hierarchy(&world, child)
                            && let Some(global) = world.get::<GlobalTransform>(child)
                        {
                            keep_world = Some((child, global.0));
                        }
                        apply_commands(&mut world, |c| {
                            c.entity(child).set_parent_keep_world_transform(parent);
                        });
                    } else {
                        apply_commands(&mut world, |c| {
                            c.entity(child).set_parent(parent);
                        });
                    }
                    let new_parent = world.get::<ChildOf>(child).map(|p| p.parent());
                    if refuse {
                        assert_eq!(new_parent, old_parent, "{context}: a cycle was not refused");
                    } else {
                        assert_eq!(new_parent, Some(parent), "{context}: a legal reparent failed");
                    }
                }
                // Detach (plain or keep-world).
                4 => {
                    let child = live[rng.below(live.len())];
                    if rng.chance(2) && in_hierarchy(&world, child) {
                        let pose = world
                            .get::<GlobalTransform>(child)
                            .map_or(Affine3A::IDENTITY, |g| g.0);
                        keep_world = Some((child, pose));
                        apply_commands(&mut world, |c| {
                            c.entity(child).remove_parent_keep_world_transform();
                        });
                    } else {
                        apply_commands(&mut world, |c| {
                            c.entity(child).remove_parent();
                        });
                    }
                    assert!(world.get::<ChildOf>(child).is_none(), "{context}");
                }
                // Edit, add or remove a transform.
                5 => {
                    let entity = live[rng.below(live.len())];
                    if world.get::<Transform>(entity).is_some() && rng.chance(4) {
                        world.entity_mut(entity).remove::<Transform>();
                    } else {
                        world.entity_mut(entity).insert(random_transform(&mut rng));
                    }
                }
                // Edit, add or remove a visibility.
                6 => {
                    let entity = live[rng.below(live.len())];
                    if world.get::<Visibility>(entity).is_some() && rng.chance(4) {
                        world.entity_mut(entity).remove::<Visibility>();
                    } else {
                        let visibility = random_visibility(&mut rng);
                        world.entity_mut(entity).insert(visibility);
                    }
                }
                // Despawn with the subtree.
                7 => {
                    let entity = live[rng.below(live.len())];
                    apply_commands(&mut world, |c| {
                        c.entity(entity).despawn_recursive();
                    });
                    live.retain(|e| world.get_entity(*e).is_ok());
                }
                // A frame with no edits.
                _ => {}
            }

            propagate_transforms(&mut world);

            for entity in &live {
                let entity = *entity;
                if in_hierarchy(&world, entity) {
                    let global = world
                        .get::<GlobalTransform>(entity)
                        .unwrap_or_else(|| panic!("{context}: {entity:?} has no GlobalTransform"))
                        .0;
                    let expected = reference_global(&world, entity);
                    assert!(
                        close(global, expected, 1e-4),
                        "{context}: {entity:?} global {global:?} != reference {expected:?}"
                    );
                }
                if world.get::<Visibility>(entity).is_some() {
                    let resolved = world.get::<InheritedVisibility>(entity).map(|v| v.0);
                    assert_eq!(
                        resolved,
                        Some(reference_visible(&world, entity)),
                        "{context}: {entity:?} visibility"
                    );
                }
                if world.get::<Transform>(entity).is_some() {
                    let previous = world.get::<PreviousGlobalTransform>(entity).map(|p| p.0);
                    let expected = before.get(&entity).copied().unwrap_or(Affine3A::IDENTITY);
                    assert_eq!(previous, Some(expected), "{context}: {entity:?} previous frame");
                }
            }
            if let Some((entity, pose)) = keep_world
                && world.get_entity(entity).is_ok()
            {
                let after = world
                    .get::<GlobalTransform>(entity)
                    .map_or(Affine3A::IDENTITY, |g| g.0);
                assert!(
                    close(after, pose, 2e-3),
                    "{context}: keep-world moved {entity:?}: {pose:?} -> {after:?}"
                );
            }
            before.clear();
            for entity in &live {
                if let Some(global) = world.get::<GlobalTransform>(*entity) {
                    before.insert(*entity, global.0);
                }
            }
        }
    }
}

fn scene_world() -> World {
    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    {
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let mut registry = handle.write();
        regia_scene::components::register_primitives(&mut registry);
        registry.register::<Transform>();
        registry.register::<Name>();
        registry.register::<EntityGuid>();
        registry.register::<Tags>();
        registry.register::<smallvec::SmallVec<[Name; 4]>>();
    }
    world
}

/// Strings that stress RON escaping and the lexical entity splitter.
const AWKWARD: [&str; 8] = [
    "Door",
    "A ] [ \" ( ) { }",
    "/* not a comment */ // nor this",
    "tab\there",
    "back\\slash",
    "\u{65e5}\u{672c}",
    "",
    "r#\"raw\"#",
];

/// What the generator authored for one entity (the reference the worlds are checked against).
#[derive(Clone, Debug)]
struct Authored {
    name: Option<String>,
    parent: Option<Guid>,
    transform: Option<Transform>,
    tags: Option<Vec<String>>,
}

/// A random valid scene: a forest in shuffled record order, with awkward names and tags.
fn random_scene(rng: &mut FuzzRng, seed: u64) -> (String, HashMap<Guid, Authored>) {
    let count = rng.range(1, 24);
    let guids: Vec<Guid> = (0..count)
        .map(|index| Guid::from_u128((u128::from(seed) << 64) | (index as u128 + 1)))
        .collect();
    let mut authored = HashMap::new();
    let mut records = Vec::new();
    for (index, guid) in guids.iter().enumerate() {
        let entity = Authored {
            name: rng
                .chance(3)
                .then(|| AWKWARD[rng.below(AWKWARD.len())].to_owned()),
            parent: (index > 0 && rng.chance(2)).then(|| guids[rng.below(index)]),
            // Most entities carry a transform; a few are bare grouping nodes.
            transform: (!rng.chance(5)).then(|| random_transform(rng)),
            tags: rng.chance(3).then(|| {
                (0..rng.range(0, 3))
                    .map(|_| AWKWARD[rng.below(AWKWARD.len())].to_owned())
                    .collect()
            }),
        };
        let mut components = Vec::new();
        if let Some(t) = &entity.transform {
            components.push(format!(
                "\"regia_scene::transform::Transform\": (translation: ({:?}, {:?}, {:?}), rotation: ({:?}, {:?}, {:?}, {:?}), scale: ({:?}, {:?}, {:?}))",
                t.translation.x,
                t.translation.y,
                t.translation.z,
                t.rotation.x,
                t.rotation.y,
                t.rotation.z,
                t.rotation.w,
                t.scale.x,
                t.scale.y,
                t.scale.z
            ));
        }
        if let Some(tags) = &entity.tags {
            let list: Vec<String> = tags.iter().map(|tag| format!("{tag:?}")).collect();
            components.push(format!(
                "\"regia_scene::components::Tags\": ([{}])",
                list.join(", ")
            ));
        }
        let mut record = format!("(guid: \"{guid}\"");
        if let Some(name) = &entity.name {
            record.push_str(&format!(", name: {name:?}"));
        }
        if let Some(parent) = entity.parent {
            // Both the bare and the `Some(...)` spellings are valid.
            if rng.chance(2) {
                record.push_str(&format!(", parent: \"{parent}\""));
            } else {
                record.push_str(&format!(", parent: Some(\"{parent}\")"));
            }
        }
        record.push_str(&format!(", components: {{ {} }})", components.join(", ")));
        records.push(record);
        authored.insert(*guid, entity);
    }
    // Shuffle the record order: parents may come after their children.
    for index in (1..records.len()).rev() {
        let other = rng.below(index + 1);
        records.swap(index, other);
    }
    let text = format!(
        "SceneFile(format_version: 10, entities: [\n    {},\n])",
        records.join(",\n    ")
    );
    (text, authored)
}

/// The authored content is what the world holds: names, parents, transforms, tags.
fn assert_world_matches(world: &mut World, authored: &HashMap<Guid, Authored>, context: &str) {
    let by_guid: HashMap<Guid, Entity> = world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .map(|(entity, guid)| (guid.0, entity))
        .collect();
    assert_eq!(by_guid.len(), authored.len(), "{context}: entity count");
    for (guid, expected) in authored {
        let entity = by_guid[guid];
        let name = world.get::<Name>(entity).map(|n| n.as_str().to_owned());
        assert_eq!(name, expected.name, "{context}: name of {guid}");
        let parent = world
            .get::<ChildOf>(entity)
            .and_then(|link| world.get::<EntityGuid>(link.parent()))
            .map(|g| g.0);
        assert_eq!(parent, expected.parent, "{context}: parent of {guid}");
        assert_eq!(
            world.get::<Transform>(entity).copied(),
            expected.transform,
            "{context}: transform of {guid}"
        );
        let tags = world
            .get::<Tags>(entity)
            .map(|tags| tags.0.iter().map(|t| t.as_str().to_owned()).collect::<Vec<_>>());
        assert_eq!(tags, expected.tags, "{context}: tags of {guid}");
    }
}

/// Entity records sorted by guid (snapshot order follows the world's query order).
fn sorted_text(mut file: SceneFile) -> String {
    file.entities.sort_by_key(|entity: &SceneEntity| entity.guid);
    file.to_ron_string().expect("write")
}

/// Random scenes survive every round trip: text → text (canonical and stable), postcard, and
/// spawn → snapshot → spawn with the authored content intact in both worlds.
#[test]
fn random_scenes_round_trip() {
    for seed in 0..48u64 {
        let mut rng = FuzzRng::new(0xF11E_0000 + seed);
        let (text, authored) = random_scene(&mut rng, seed);
        let context = format!("seed {seed}");
        let file = SceneFile::from_ron_str(&text).unwrap_or_else(|err| panic!("{context}: {err}\n{text}"));
        assert_eq!(file.entities.len(), authored.len(), "{context}");

        let canonical = file.to_ron_string().expect("write");
        let reparsed = SceneFile::from_ron_str(&canonical).unwrap_or_else(|err| panic!("{context}: {err}"));
        assert_eq!(reparsed, file, "{context}: text round trip");
        assert_eq!(
            reparsed.to_ron_string().expect("write"),
            canonical,
            "{context}: canonical text is stable"
        );
        let bytes = file.to_postcard().expect("postcard");
        assert_eq!(
            SceneFile::from_postcard(&bytes).expect("unpostcard"),
            file,
            "{context}: postcard round trip"
        );

        let mut world = scene_world();
        let spawned = spawn_scene(&mut world, &file, &SpawnOptions::default()).expect("spawn");
        assert!(spawned.warnings.is_empty(), "{context}: {:?}", spawned.warnings);
        assert_world_matches(&mut world, &authored, &format!("{context} (world 1)"));

        let snapshot = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
        let snapshot_text = snapshot.to_ron_string().expect("write");
        let snapshot = SceneFile::from_ron_str(&snapshot_text).expect("snapshot parses");
        let mut world2 = scene_world();
        spawn_scene(&mut world2, &snapshot, &SpawnOptions::default()).expect("respawn");
        assert_world_matches(&mut world2, &authored, &format!("{context} (world 2)"));
        let snapshot2 = snapshot_scene(&mut world2, &SnapshotFilter::default()).expect("snapshot 2");
        assert_eq!(
            sorted_text(snapshot2),
            sorted_text(snapshot),
            "{context}: snapshot round trip is stable"
        );
    }
}

/// The naive cycle check: walk up at most `n` steps from every entity.
fn naive_is_valid(parents: &[Option<usize>], unknown: &[bool]) -> bool {
    let n = parents.len();
    for (start, is_unknown) in unknown.iter().enumerate() {
        if *is_unknown {
            return false;
        }
        let mut cursor = start;
        let mut steps = 0;
        while let Some(parent) = parents[cursor] {
            if parent == start {
                return false;
            }
            cursor = parent;
            steps += 1;
            if steps > n {
                return false;
            }
        }
    }
    true
}

/// `SceneFile::validate` (linear, memoized) agrees with the naive walk on random parent maps,
/// including self-parents, long cycles, lassos and unknown parents.
#[test]
fn validate_agrees_with_a_naive_cycle_check() {
    let mut invalid = 0;
    for seed in 0..400u64 {
        let mut rng = FuzzRng::new(0xC1C1_0000 + seed);
        let n = rng.range(1, 12);
        let guid = |index: usize| Guid::from_u128(index as u128 + 1);
        let parents: Vec<Option<usize>> = (0..n).map(|_| (!rng.chance(3)).then(|| rng.below(n))).collect();
        let unknown: Vec<bool> = (0..n).map(|_| rng.chance(25)).collect();
        let mut file = SceneFile::new();
        for (index, (parent, is_unknown)) in parents.iter().zip(&unknown).enumerate() {
            file.entities.push(SceneEntity {
                guid: guid(index),
                parent: if *is_unknown {
                    Some(Guid::from_u128(0xDEAD))
                } else {
                    parent.map(guid)
                },
                ..SceneEntity::default()
            });
        }
        let expected = naive_is_valid(&parents, &unknown);
        if !expected {
            invalid += 1;
        }
        assert_eq!(
            file.validate().is_ok(),
            expected,
            "seed {seed}: parents {parents:?} unknown {unknown:?}"
        );
    }
    assert!(
        invalid > 50,
        "the generator must produce invalid maps too ({invalid})"
    );
}
