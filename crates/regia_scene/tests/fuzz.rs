// SPDX-License-Identifier: MPL-2.0
//! M18 robustness: mutation fuzzing of the scene-file loaders (`.level.ron`, `.prefab.ron` and
//! the cooked form). See `docs/evidence/m18/robustness.md`.
//!
//! The ADR-538 targets go past the parse: every mutation that loads is spawned into a world
//! with the prefab fixture baked, transforms are propagated and the world is snapshotted back
//! to a scene file, so hostile hierarchies, prefab instances and component values reach the
//! code that walks them. Cooked bytes skip validation, so their parent links are checked for
//! cycles at spawn.

// `SceneFormatError` embeds `ron::SpannedError` (`result_large_err` on the test closure).
#![allow(clippy::result_large_err)]

use std::panic::AssertUnwindSafe;

use bevy_ecs::hierarchy::ChildOf;
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::components::Name;
use regia_scene::cooked::{CookedEntity, CookedParent, CookedScene, cook_scene};
use regia_scene::format::{PrefabInstanceData, SceneAssetRef, SceneEntity, SceneFile};
use regia_scene::prefab::{PrefabLibrary, PrefabTemplate};
use regia_scene::propagate::propagate_transforms;
use regia_scene::spawn::{SnapshotFilter, SpawnOptions, snapshot_scene, spawn_scene};
use regia_scene::transform::Transform;
use regia_test_utils::fuzz::{assert_no_panic, fuzz_loader, fuzz_text_loader};

const N: usize = 3_000;

#[test]
fn level_ron_never_panics() {
    let fixture = include_str!("fixtures/level.example.ron");
    fuzz_text_loader(fixture, 0xB1, N, SceneFile::from_ron_str);
}

#[test]
fn prefab_ron_never_panics() {
    let fixture = include_str!("fixtures/prefab.example.ron");
    fuzz_text_loader(fixture, 0xB2, N, SceneFile::from_ron_str);
}

#[test]
fn scene_postcard_never_panics() {
    let file = SceneFile::from_ron_str(include_str!("fixtures/level.example.ron")).expect("parse");
    let fixture = file.to_postcard().expect("postcard");
    fuzz_loader(&fixture, 0xB3, N, SceneFile::from_postcard);
}

#[test]
fn cooked_scene_never_panics() {
    let fixture = CookedScene {
        format_version: 1,
        strings: vec![],
        entities: vec![],
        resources: vec![],
    }
    .to_bytes()
    .expect("bytes");
    fuzz_loader(&fixture, 0xB4, N, CookedScene::from_bytes);
}

// ---- ADR-538: load, spawn, walk ---------------------------------------------------------------

/// The asset id the level fixture's prefab instance names; the prefab fixture is baked under it
/// so the instance (and its override and added component) resolves.
const PREFAB_ASSET: &str = "44fe6f0a-1e42-4c11-8d5a-9b3c2f7e10d0";

/// A world with the scene types registered and the prefab fixture baked.
fn spawn_world() -> World {
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
    let prefab =
        SceneFile::from_ron_str(include_str!("fixtures/prefab.example.ron")).expect("prefab fixture");
    let asset: Guid = PREFAB_ASSET.parse().expect("prefab asset guid");
    let registry = world.resource::<AppTypeRegistry>().0.clone();
    let template = PrefabTemplate::from_scene_file(asset, &prefab, &registry.read()).expect("bake prefab");
    world.resource_mut::<PrefabLibrary>().insert(template);
    world
}

/// No entity's parent chain loops: each walk up ends within the entity count.
fn assert_hierarchy_is_acyclic(world: &mut World) {
    let entities: Vec<Entity> = world.query::<Entity>().iter(world).collect();
    for &start in &entities {
        let mut cursor = start;
        let mut steps = 0usize;
        while let Some(parent) = world.get::<ChildOf>(cursor).map(ChildOf::parent) {
            cursor = parent;
            steps += 1;
            assert!(steps <= entities.len(), "{start:?}: parent chain loops");
        }
    }
}

/// Spawns `file`, propagates transforms and snapshots the world back to a scene file.
fn spawn_and_walk(file: &SceneFile) -> Result<usize, String> {
    let mut world = spawn_world();
    let spawned = spawn_scene(&mut world, file, &SpawnOptions::default()).map_err(|e| e.to_string())?;
    propagate_transforms(&mut world);
    assert_hierarchy_is_acyclic(&mut world);
    let _ = snapshot_scene(&mut world, &SnapshotFilter::default());
    Ok(spawned.entities)
}

fn parse_and_spawn(source: &str) -> Result<usize, String> {
    let file = SceneFile::from_ron_str(source).map_err(|e| e.to_string())?;
    spawn_and_walk(&file)
}

fn postcard_and_spawn(bytes: &[u8]) -> Result<usize, String> {
    let file = SceneFile::from_postcard(bytes).map_err(|e| e.to_string())?;
    spawn_and_walk(&file)
}

/// The cooked runtime path: decode, spawn, propagate.
fn cooked_and_spawn(bytes: &[u8]) -> Result<usize, String> {
    let cooked = CookedScene::from_bytes(bytes).map_err(|e| e.to_string())?;
    let mut world = spawn_world();
    let spawned = cooked
        .spawn(&mut world, &SpawnOptions::default())
        .map_err(|e| e.to_string())?;
    propagate_transforms(&mut world);
    assert_hierarchy_is_acyclic(&mut world);
    Ok(spawned.entities)
}

#[test]
fn level_ron_spawns_never_panic() {
    let fixture = include_str!("fixtures/level.example.ron");
    // The fixture reaches the prefab instantiation (prefab root + three children + the level's).
    assert!(parse_and_spawn(fixture).expect("fixture spawns") > 4);
    fuzz_text_loader(fixture, 0x0005_3831, N, parse_and_spawn);
}

#[test]
fn prefab_ron_spawns_never_panic() {
    let fixture = include_str!("fixtures/prefab.example.ron");
    assert!(parse_and_spawn(fixture).expect("fixture spawns") >= 4);
    fuzz_text_loader(fixture, 0x0005_3832, N, parse_and_spawn);
}

#[test]
fn scene_postcard_spawns_never_panic() {
    let file = SceneFile::from_ron_str(include_str!("fixtures/level.example.ron")).expect("parse");
    let fixture = file.to_postcard().expect("postcard");
    postcard_and_spawn(&fixture).expect("fixture spawns");
    fuzz_loader(&fixture, 0x0005_3833, N, postcard_and_spawn);
}

#[test]
fn cooked_level_spawns_never_panic() {
    let world = spawn_world();
    let file = SceneFile::from_ron_str(include_str!("fixtures/level.example.ron")).expect("parse");
    let (cooked, _warnings) = cook_scene(&world, &file).expect("cook");
    let fixture = cooked.to_bytes().expect("bytes");
    assert!(cooked_and_spawn(&fixture).expect("fixture spawns") > 4);
    fuzz_loader(&fixture, 0x0005_3834, N, cooked_and_spawn);
}

/// Cooked parent links skip the scene validator: a self-parent, a two-entity loop, a chain into
/// that loop and a repeated guid all spawn with warnings, and no parent chain loops.
#[test]
fn cooked_parent_cycles_are_broken_at_spawn() {
    let row = |guid: u128, parent: Option<u32>| CookedEntity {
        guid: Guid::from_u128(guid),
        name: None,
        parent: parent.map(CookedParent::Index),
        components: Vec::new(),
        prefab: None,
    };
    let scene = CookedScene {
        format_version: 1,
        strings: Vec::new(),
        entities: vec![
            row(1, Some(0)),
            row(2, Some(2)),
            row(3, Some(1)),
            row(4, Some(2)),
            row(1, Some(3)),
            row(6, Some(99)),
        ],
        resources: Vec::new(),
    };
    let mut world = spawn_world();
    let spawned = assert_no_panic(
        "cooked cycles",
        AssertUnwindSafe(|| scene.spawn(&mut world, &SpawnOptions::default())),
    )
    .expect("spawns");
    assert_eq!(spawned.entities, 6);
    assert!(
        spawned.warnings.iter().filter(|w| w.contains("cycle")).count() >= 2,
        "{:?}",
        spawned.warnings
    );
    propagate_transforms(&mut world);
    assert_hierarchy_is_acyclic(&mut world);
}

/// `levels` prefab instances, each the single added child of the next.
fn nested_instance(levels: u128) -> PrefabInstanceData {
    let asset = SceneAssetRef::new(PREFAB_ASSET.parse().expect("guid"));
    let mut instance = PrefabInstanceData::plain(asset);
    for level in 0..levels {
        let child = SceneEntity {
            guid: Guid::from_u128(level + 1),
            name: None,
            parent: None,
            components: Default::default(),
            prefab_instance: Some(instance),
        };
        instance = PrefabInstanceData::plain(asset);
        instance.added_children.push(child);
    }
    instance
}

/// Postcard has no recursion limit of its own; `added_children` nested past the prefab nesting
/// limit is a decode error (in the authoring and the cooked form), not a stack overflow.
#[test]
fn deeply_nested_added_children_are_rejected() {
    let shallow = postcard::to_allocvec(&nested_instance(10)).expect("encode");
    assert!(
        postcard::from_bytes::<PrefabInstanceData>(&shallow).is_ok(),
        "10 levels decode"
    );
    let deep = postcard::to_allocvec(&nested_instance(100)).expect("encode");
    assert!(
        postcard::from_bytes::<PrefabInstanceData>(&deep).is_err(),
        "100 levels are past the limit"
    );
    let cooked = CookedScene {
        format_version: 1,
        strings: Vec::new(),
        entities: vec![CookedEntity {
            guid: Guid::from_u128(1),
            name: None,
            parent: None,
            components: Vec::new(),
            prefab: Some(nested_instance(100)),
        }],
        resources: Vec::new(),
    };
    let bytes = cooked.to_bytes().expect("cooked bytes");
    assert!(
        CookedScene::from_bytes(&bytes).is_err(),
        "cooked nesting past the limit is rejected"
    );
}

/// ADR-1917: a component written by a newer build used to load as if it were current, because nothing on the spawn
/// path looked at its `format_version`. It stays out of the world with a warning; under `Preserve` its text survives
/// for the next save. A current one spawns.
#[test]
fn a_component_from_a_newer_build_is_left_out_and_kept_for_saving() {
    use regia_ecs::prelude::PartialReflect;
    use regia_ecs::serialize::ReflectCodec;
    use regia_scene::spawn::UnknownTypePolicyOption;
    use regia_scene::spline::{SPLINE_FORMAT_VERSION, Spline, SplinePoint, SplineUp, TangentMode};
    let level = |world: &World, format_version: u32| {
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let registry = registry.read();
        let spline = Spline {
            format_version,
            ..Spline::default()
        };
        let body = ReflectCodec::new(&registry)
            .to_ron_typed(&spline as &dyn PartialReflect)
            .expect("the component encodes");
        format!(
            r#"(format_version: 10, entities: [(guid: "8f1c3a00-0000-0000-0000-000000000001", name: "Road",
                components: {{"regia_scene::Spline": {body}}})])"#
        )
    };
    for (version, spawns) in [(SPLINE_FORMAT_VERSION, true), (9, false)] {
        let mut world = spawn_world();
        {
            let handle = world.resource::<AppTypeRegistry>().0.clone();
            let mut registry = handle.write();
            registry.register::<Spline>();
            registry.register::<SplinePoint>();
            registry.register::<SplineUp>();
            registry.register::<TangentMode>();
        }
        let file = SceneFile::from_ron_str(&level(&world, version)).expect("parses");
        let options = SpawnOptions {
            unknown_types: UnknownTypePolicyOption::Preserve,
            ..SpawnOptions::default()
        };
        let spawned = spawn_scene(&mut world, &file, &options).expect("spawns");
        let mut splines = world.query::<&Spline>();
        assert_eq!(splines.iter(&world).count() == 1, spawns, "version {version}");
        assert_eq!(
            spawned.warnings.iter().any(|w| w.contains("is newer than")),
            !spawns,
            "{:?}",
            spawned.warnings
        );
        if !spawns {
            let saved = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
            let kept = saved.entities[0]
                .components
                .get("regia_scene::Spline")
                .expect("the text is kept for the next save");
            assert!(kept.get_ron().contains("format_version: 9"), "{}", kept.get_ron());
        }
    }
}
