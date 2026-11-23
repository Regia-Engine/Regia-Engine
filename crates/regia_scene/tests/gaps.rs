// SPDX-License-Identifier: MPL-2.0
//! Error paths and option handling of the scene crate's public API that the module tests and
//! `formats.rs` do not reach: load failures, option tagging, snapshot filtering, the cooked
//! container's defensive paths, and `Transform` / hierarchy edge cases.

use bevy_ecs::hierarchy::ChildOf;
use bevy_ecs::world::CommandQueue;
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::{Affine3A, Quat, Vec3};
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::components::Name;
use regia_scene::cooked::{COOKED_SCENE_VERSION, CookedScene};
use regia_scene::format::{SCENE_FORMAT_VERSION, SceneEntity, SceneFile, SceneFormatError};
use regia_scene::level::{LevelId, LevelMember, Persistent};
use regia_scene::prefab::PrefabLibrary;
use regia_scene::prelude::*;
use regia_scene::spawn::{SceneSpawnError, SnapshotFilter, SpawnOptions, snapshot_scene, spawn_scene};

const TRANSFORM_PATH: &str = "regia_scene::transform::Transform";

/// A world with the scene types registered (what `ScenePlugin` would set up).
fn test_world() -> World {
    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    {
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let mut registry = handle.write();
        regia_scene::components::register_primitives(&mut registry);
        registry.register::<Transform>();
        registry.register::<Name>();
    }
    world.init_resource::<PrefabLibrary>();
    world
}

/// A single-entity scene text: `guid`, a name and a translated `Transform`.
fn one_entity_text(guid: Guid, name: &str) -> String {
    format!(
        r#"
(
    format_version: 1,
    entities: [
        (guid: "{guid}", name: "{name}", components: {{
            "{TRANSFORM_PATH}": (translation: (1.0, 2.0, 3.0)),
        }}),
    ],
)
"#
    )
}

fn entity_with_guid(world: &mut World, guid: Guid) -> Option<Entity> {
    world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(_, g)| g.0 == guid)
        .map(|(entity, _)| entity)
}

fn guid_count(world: &mut World) -> usize {
    world.query::<&EntityGuid>().iter(world).count()
}

// ---- format: load failures --------------------------------------------------------------------

/// Text that is not a scene document fails to load rather than yielding an empty scene.
#[test]
fn garbage_text_fails_to_load() {
    assert!(SceneFile::from_ron_str("this is not a scene file").is_err());
    assert!(SceneFile::from_ron_str("").is_err());
}

/// A record with a malformed guid string is a parse error, not a silent nil guid.
#[test]
fn malformed_guid_text_fails_to_load() {
    let text = r#"(format_version: 1, entities: [(guid: "not-a-guid")])"#;
    assert!(SceneFile::from_ron_str(text).is_err());
}

/// Structural validation runs on load: duplicate guids, nil guids and unknown parents are
/// `Invalid`, and the message names the problem.
#[test]
fn structural_errors_in_text_are_reported_as_invalid() {
    let a = Guid::from_u128(0xA);
    let b = Guid::from_u128(0xB);

    let duplicate = format!(r#"(format_version: 1, entities: [(guid: "{a}"), (guid: "{a}")])"#);
    match SceneFile::from_ron_str(&duplicate) {
        Err(SceneFormatError::Invalid(message)) => assert!(message.contains("duplicate"), "{message}"),
        other => panic!("expected Invalid(duplicate), got {other:?}"),
    }

    let orphan = format!(r#"(format_version: 1, entities: [(guid: "{a}", parent: Some("{b}"))])"#);
    match SceneFile::from_ron_str(&orphan) {
        Err(SceneFormatError::Invalid(message)) => {
            assert!(message.contains("unknown parent"), "{message}");
            assert!(
                message.contains(&b.to_string()),
                "names the missing parent: {message}"
            );
        }
        other => panic!("expected Invalid(unknown parent), got {other:?}"),
    }

    let nil = format!(r#"(format_version: 1, entities: [(guid: "{}")])"#, Guid::NIL);
    assert!(matches!(
        SceneFile::from_ron_str(&nil),
        Err(SceneFormatError::Invalid(_))
    ));
}

/// The postcard reader applies the same version rule as the text reader.
#[test]
fn postcard_newer_version_is_rejected_with_both_versions() {
    let mut file = SceneFile::new();
    file.format_version = SCENE_FORMAT_VERSION + 1;
    let bytes = file.to_postcard().expect("serialize");
    match SceneFile::from_postcard(&bytes) {
        Err(SceneFormatError::UnsupportedVersion { found, supported }) => {
            assert_eq!(found, SCENE_FORMAT_VERSION + 1);
            assert_eq!(supported, SCENE_FORMAT_VERSION);
        }
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }
}

/// Levels audit L4: a newer writer may change the body, so the version is read before the body: the
/// reader must say `UnsupportedVersion`, not a generic decode error.
#[test]
fn postcard_newer_version_is_named_even_when_its_body_does_not_decode() {
    let mut bytes = postcard::to_allocvec(&(SCENE_FORMAT_VERSION + 1)).expect("varint");
    bytes.extend_from_slice(&[0xff; 16]);
    assert!(matches!(
        SceneFile::from_postcard(&bytes),
        Err(SceneFormatError::UnsupportedVersion { .. })
    ));
}

/// Levels audit L3: validation is per file, so two copy-pasted levels opened together share a guid
/// and guid-keyed lookups pick either entity. The spawn still happens; it says so.
#[test]
fn a_guid_already_in_the_world_is_reported() {
    let mut world = test_world();
    let guid = Guid::from_u128(0x77);
    let file = SceneFile::from_ron_str(&one_entity_text(guid, "Prop")).expect("parse");
    let first = spawn_scene(&mut world, &file, &SpawnOptions::default()).expect("first");
    assert!(first.warnings.is_empty(), "{:?}", first.warnings);
    let second = spawn_scene(&mut world, &file, &SpawnOptions::default()).expect("second");
    assert_eq!(second.entities, 1);
    assert!(
        second.warnings.iter().any(|w| w.contains("already in the world")),
        "{:?}",
        second.warnings
    );
}

/// Truncated or garbage postcard is an error, never a partial scene.
#[test]
fn truncated_and_garbage_postcard_fail() {
    let file = SceneFile::from_ron_str(&one_entity_text(Guid::from_u128(1), "Prop")).expect("parse");
    let bytes = file.to_postcard().expect("serialize");
    assert!(bytes.len() > 4);
    assert!(SceneFile::from_postcard(&bytes[..bytes.len() / 2]).is_err());
    assert!(SceneFile::from_postcard(&[]).is_err());
    assert!(SceneFile::from_postcard(&[0xff; 3]).is_err());
}

/// An empty current-version file loads, validates and round trips through both encodings.
#[test]
fn empty_scene_round_trips_through_text_and_postcard() {
    let file = SceneFile::new();
    assert_eq!(file.format_version, SCENE_FORMAT_VERSION);
    let text = file.to_ron_string().expect("write");
    assert_eq!(SceneFile::from_ron_str(&text).expect("read"), file);
    let bytes = file.to_postcard().expect("postcard");
    assert_eq!(SceneFile::from_postcard(&bytes).expect("unpostcard"), file);
}

// ---- spawn: options and failure ---------------------------------------------------------------

/// `spawn_scene` validates first: an invalid file returns an error and spawns nothing.
#[test]
fn spawn_of_an_invalid_file_spawns_nothing() {
    let mut world = test_world();
    let mut file = SceneFile::new();
    let record = SceneEntity {
        guid: Guid::from_u128(5),
        ..SceneEntity::default()
    };
    file.entities = vec![record.clone(), record];

    let result = spawn_scene(&mut world, &file, &SpawnOptions::default());
    assert!(matches!(result, Err(SceneSpawnError::Format(_))), "{result:?}");
    assert_eq!(guid_count(&mut world), 0, "validation happens before any spawn");
}

/// `SpawnOptions::level` and `persistent` tag every spawned entity; the defaults tag none.
#[test]
fn spawn_options_tag_entities() {
    let guid = Guid::from_u128(0x10);
    let file = SceneFile::from_ron_str(&one_entity_text(guid, "Prop")).expect("parse");
    let level = LevelId(Guid::from_u128(0x77));

    let mut plain = test_world();
    spawn_scene(&mut plain, &file, &SpawnOptions::default()).expect("spawn");
    let entity = entity_with_guid(&mut plain, guid).expect("entity");
    assert!(plain.get::<LevelMember>(entity).is_none());
    assert!(plain.get::<Persistent>(entity).is_none());
    assert_eq!(plain.get::<Name>(entity).map(|n| n.as_str()), Some("Prop"));

    let mut tagged = test_world();
    let opts = SpawnOptions {
        level: Some(level),
        persistent: true,
        ..SpawnOptions::default()
    };
    spawn_scene(&mut tagged, &file, &opts).expect("spawn");
    let entity = entity_with_guid(&mut tagged, guid).expect("entity");
    assert_eq!(tagged.get::<LevelMember>(entity).map(|m| m.level()), Some(level));
    assert!(tagged.get::<Persistent>(entity).is_some());
}

/// Roots in the report are exactly the entities without a parent, and parenting is applied.
#[test]
fn spawn_reports_only_parentless_entities_as_roots() {
    let mut world = test_world();
    let root = Guid::from_u128(1);
    let child = Guid::from_u128(2);
    let grandchild = Guid::from_u128(3);
    let text = format!(
        r#"(format_version: 1, entities: [
            (guid: "{grandchild}", parent: Some("{child}")),
            (guid: "{child}", parent: Some("{root}")),
            (guid: "{root}"),
        ])"#
    );
    let file = SceneFile::from_ron_str(&text).expect("parse");
    let spawned = spawn_scene(&mut world, &file, &SpawnOptions::default()).expect("spawn");
    assert_eq!(spawned.entities, 3);
    assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);
    let root_entity = entity_with_guid(&mut world, root).expect("root");
    assert_eq!(
        spawned.roots,
        vec![root_entity],
        "file order does not matter for parenting"
    );
    let child_entity = entity_with_guid(&mut world, child).expect("child");
    let grandchild_entity = entity_with_guid(&mut world, grandchild).expect("grandchild");
    assert_eq!(
        world.get::<ChildOf>(child_entity).map(|c| c.parent()),
        Some(root_entity)
    );
    assert_eq!(
        world.get::<ChildOf>(grandchild_entity).map(|c| c.parent()),
        Some(child_entity)
    );
}

/// A snapshot filtered to one level leaves the other level's entities out.
#[test]
fn snapshot_filter_by_level_excludes_other_levels() {
    let mut world = test_world();
    let (a, b) = (Guid::from_u128(0xA1), Guid::from_u128(0xB2));
    let (level_a, level_b) = (LevelId(Guid::from_u128(0xAA)), LevelId(Guid::from_u128(0xBB)));
    for (guid, level) in [(a, level_a), (b, level_b)] {
        let file = SceneFile::from_ron_str(&one_entity_text(guid, "Prop")).expect("parse");
        let opts = SpawnOptions {
            level: Some(level),
            ..SpawnOptions::default()
        };
        spawn_scene(&mut world, &file, &opts).expect("spawn");
    }

    let only_a = snapshot_scene(
        &mut world,
        &SnapshotFilter {
            level: Some(level_a),
            ..SnapshotFilter::default()
        },
    )
    .expect("snapshot");
    let guids: Vec<Guid> = only_a.entities.iter().map(|e| e.guid).collect();
    assert_eq!(guids, vec![a]);

    let everything = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
    assert_eq!(everything.entities.len(), 2);
}

/// A snapshot naming a resource type that has no hook is skipped, not an error.
#[test]
fn snapshot_ignores_unhooked_resource_paths() {
    let mut world = test_world();
    let filter = SnapshotFilter {
        resources: vec!["no_such_crate::Missing".into()],
        ..SnapshotFilter::default()
    };
    let file = snapshot_scene(&mut world, &filter).expect("snapshot");
    assert!(file.resources.is_empty());
}

// ---- cooked container -------------------------------------------------------------------------

/// String-table lookups are bounds checked.
#[test]
fn cooked_string_lookup_is_bounds_checked() {
    let cooked = CookedScene {
        format_version: COOKED_SCENE_VERSION,
        strings: vec!["a".into(), "b".into()],
        entities: vec![],
        resources: vec![],
    };
    assert_eq!(cooked.string(0), Some("a"));
    assert_eq!(cooked.string(1), Some("b"));
    assert_eq!(cooked.string(2), None);
    assert_eq!(cooked.string(u32::MAX), None);
}

/// Garbage bytes are an error from the cooked reader; an empty current-version scene spawns
/// zero entities without warnings.
#[test]
fn cooked_reader_rejects_garbage_and_spawns_empty_scenes() {
    assert!(CookedScene::from_bytes(&[0xff; 3]).is_err());
    assert!(CookedScene::from_bytes(&[]).is_err());

    let cooked = CookedScene {
        format_version: COOKED_SCENE_VERSION,
        strings: vec![],
        entities: vec![],
        resources: vec![],
    };
    let bytes = cooked.to_bytes().expect("bytes");
    let parsed = CookedScene::from_bytes(&bytes).expect("parse");
    let mut world = test_world();
    let spawned = parsed.spawn(&mut world, &SpawnOptions::default()).expect("spawn");
    assert_eq!(spawned.entities, 0);
    assert!(spawned.roots.is_empty());
    assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);
}

/// Cooked spawning honors the same level and persistence tags as the text path.
#[test]
fn cooked_spawn_applies_level_and_persistent_tags() {
    let guid = Guid::from_u128(0x42);
    let file = SceneFile::from_ron_str(&one_entity_text(guid, "Prop")).expect("parse");
    let level = LevelId(Guid::from_u128(0x99));
    let mut world = test_world();
    let opts = SpawnOptions {
        level: Some(level),
        persistent: true,
        ..SpawnOptions::default()
    };
    let (_, spawned) = regia_scene::cooked::round_trip_cooked(&mut world, &file, &opts).expect("cooked");
    assert_eq!(spawned.entities, 1);
    let entity = entity_with_guid(&mut world, guid).expect("entity");
    assert_eq!(world.get::<LevelMember>(entity).map(|m| m.level()), Some(level));
    assert!(world.get::<Persistent>(entity).is_some());
    let transform = world.get::<Transform>(entity).expect("transform");
    assert_eq!(transform.translation, Vec3::new(1.0, 2.0, 3.0));
}

// ---- transform --------------------------------------------------------------------------------

/// Scale applies before rotation before translation (the documented TRS order).
#[test]
fn transform_applies_scale_then_rotation_then_translation() {
    let t = Transform {
        translation: Vec3::new(10.0, 0.0, 0.0),
        rotation: Quat::from_rotation_z(90f32.to_radians()),
        scale: Vec3::new(2.0, 1.0, 1.0),
    };
    // X scaled to 2, rotated 90 degrees about Z onto +Y, then translated by +10 in X.
    let p = t.transform_point(Vec3::X);
    assert!((p - Vec3::new(10.0, 2.0, 0.0)).length() < 1e-5, "{p:?}");
}

/// A degenerate (zero-scale) matrix decomposes without NaN and keeps its translation.
#[test]
fn from_affine_of_a_singular_matrix_stays_finite() {
    let t = Transform {
        translation: Vec3::new(4.0, 5.0, 6.0),
        rotation: Quat::IDENTITY,
        scale: Vec3::new(0.0, 1.0, 1.0),
    };
    let back = Transform::from_affine(t.compute_affine());
    assert_eq!(back.translation, Vec3::new(4.0, 5.0, 6.0));
    assert!(back.rotation.is_finite(), "{back:?}");
    assert!(back.scale.is_finite(), "{back:?}");
    assert_eq!(back.scale.y, 1.0);
    assert_eq!(back.scale.z, 1.0);
}

/// `GlobalTransform::to_transform` inverts `mul_transform` for a pure TRS child under a
/// translated parent.
#[test]
fn global_transform_composes_and_decomposes() {
    let parent = GlobalTransform(Affine3A::from_translation(Vec3::new(0.0, 7.0, 0.0)));
    let local = Transform {
        translation: Vec3::new(1.0, 0.0, 0.0),
        rotation: Quat::from_rotation_y(0.5),
        scale: Vec3::splat(3.0),
    };
    let world = parent.mul_transform(&local);
    assert!((world.translation() - Vec3::new(1.0, 7.0, 0.0)).length() < 1e-5);
    let decomposed = world.to_transform();
    assert!((decomposed.scale - Vec3::splat(3.0)).length() < 1e-5);
    assert!(decomposed.rotation.angle_between(local.rotation) < 1e-4);
}

// ---- hierarchy --------------------------------------------------------------------------------

/// `set_parent_keep_world_transform` with a parent that does not exist leaves the child alone.
#[test]
fn keep_world_reparent_to_a_missing_parent_is_a_no_op() {
    let mut world = World::new();
    let ghost = world.spawn_empty().id();
    world.despawn(ghost);
    let child = world
        .spawn(Transform::from_translation(Vec3::new(1.0, 2.0, 3.0)))
        .id();

    let mut queue = CommandQueue::default();
    {
        let mut commands = Commands::new(&mut queue, &world);
        commands.entity(child).set_parent_keep_world_transform(ghost);
    }
    queue.apply(&mut world);

    assert!(world.get::<ChildOf>(child).is_none());
    assert_eq!(
        world.get::<Transform>(child).map(|t| t.translation),
        Some(Vec3::new(1.0, 2.0, 3.0))
    );
}

/// `local_under_parent` returns the identity when the entity has no `GlobalTransform`.
#[test]
fn local_under_parent_without_a_global_is_identity() {
    let mut world = World::new();
    let bare = world.spawn_empty().id();
    let local = regia_scene::hierarchy::local_under_parent(&world, bare, None);
    assert_eq!(local, Transform::IDENTITY);
}

/// Plain `remove_parent` keeps the local `Transform`, so the world pose changes.
#[test]
fn plain_remove_parent_keeps_the_local_transform() {
    let mut world = World::new();
    let parent = world
        .spawn(Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)))
        .id();
    let child = world
        .spawn(Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)))
        .insert(ChildOf(parent))
        .id();
    propagate_transforms(&mut world);
    assert_eq!(
        world.get::<GlobalTransform>(child).map(|g| g.translation()),
        Some(Vec3::new(6.0, 0.0, 0.0))
    );

    let mut queue = CommandQueue::default();
    {
        let mut commands = Commands::new(&mut queue, &world);
        commands.entity(child).remove_parent();
    }
    queue.apply(&mut world);
    propagate_transforms(&mut world);

    assert!(world.get::<ChildOf>(child).is_none());
    assert_eq!(
        world.get::<GlobalTransform>(child).map(|g| g.translation()),
        Some(Vec3::new(1.0, 0.0, 0.0))
    );
}
