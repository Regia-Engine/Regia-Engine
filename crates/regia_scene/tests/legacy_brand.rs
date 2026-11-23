// SPDX-License-Identifier: MPL-2.0
//! Levels written before the rename to Regia Engine (ADR-1388) name their components by the old
//! crate names (`<old prefix>scene::transform::Transform`). They must keep loading: the spawn path
//! retries an unknown type path with the current crate names (`regia_core::brand`).

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::components::{Name, Tags};
use regia_scene::format::SceneFile;
use regia_scene::prefab::PrefabLibrary;
use regia_scene::spawn::{SpawnOptions, spawn_scene};
use regia_scene::transform::Transform;

/// Kept byte for byte by the rename tool.
const LEGACY_LEVEL: &str = include_str!("fixtures/legacy-brand/yard.level.ron");

fn world() -> World {
    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    {
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let mut registry = handle.write();
        regia_scene::components::register_primitives(&mut registry);
        registry.register::<Transform>();
        registry.register::<Name>();
        registry.register::<Tags>();
        registry.register::<smallvec::SmallVec<[Name; 4]>>();
    }
    world.init_resource::<PrefabLibrary>();
    world
}

#[test]
fn the_fixture_names_the_legacy_crates() {
    let legacy = regia_core::brand::LEGACY_CRATE_PREFIX;
    assert!(LEGACY_LEVEL.contains(&format!("\"{legacy}scene::transform::Transform\"")));
    assert!(LEGACY_LEVEL.contains(&format!("\"{legacy}scene::components::Tags\"")));
}

#[test]
fn a_level_with_legacy_type_paths_spawns_every_component() {
    let mut world = world();
    let file = SceneFile::from_ron_str(LEGACY_LEVEL).expect("legacy level parses");
    let spawned = spawn_scene(&mut world, &file, &SpawnOptions::default()).expect("spawn");
    assert!(
        spawned.warnings.is_empty(),
        "no component dropped: {:?}",
        spawned.warnings
    );
    assert_eq!(spawned.roots.len(), 1);
    let root = spawned.roots[0];
    let transform = world
        .get::<Transform>(root)
        .expect("Transform from the legacy key");
    assert_eq!(transform.translation, Vec3::new(1.0, 2.0, 3.0));
    assert!(world.get::<Tags>(root).is_some(), "Tags from the legacy key");
}

/// A level written by the v0.1.29 engine (`Level::to_ron_string` with non-default settings,
/// scratch writer in a v0.1.29 checkout, rename audit), kept byte for byte.
const LEGACY_SETTINGS_LEVEL: &str = include_str!("fixtures/legacy-brand/settings.level.ron");

fn level_id() -> regia_scene::level::LevelId {
    regia_scene::level::LevelId(regia_core::id::Guid::new_random())
}

#[test]
fn level_settings_written_before_the_rename_are_read() {
    let legacy = regia_core::brand::LEGACY_CRATE_PREFIX;
    assert!(LEGACY_SETTINGS_LEVEL.contains(&format!("\"{legacy}scene::level::LevelSettings\"")));
    let level = regia_scene::level::Level::from_ron_str(level_id(), LEGACY_SETTINGS_LEVEL).expect("parses");
    assert_eq!(
        level.settings.gravity,
        Vec3::new(0.0, -3.71, 0.0),
        "the settings come from the legacy key, not the defaults"
    );
    assert_eq!(
        level.settings.atmosphere.preset,
        regia_scene::level::AtmospherePreset::Airless
    );
    // Saved again, the file names the current crate.
    let text = level.to_ron_string().expect("save");
    assert!(text.contains(regia_scene::level::LEVEL_SETTINGS_PATH));
    if regia_core::brand::BRAND_RENAMED {
        assert!(!text.contains(&format!("\"{legacy}scene::level")));
    }
}

#[test]
fn an_old_format_level_with_legacy_settings_migrates_them() {
    // DO27's exported maps are format v3 with the settings under the legacy key.
    let legacy = regia_core::brand::LEGACY_CRATE_PREFIX;
    let text = format!(
        r#"(
    format_version: 3,
    entities: [],
    resources: {{
        "{legacy}scene::level::LevelSettings": (gravity: (0.0, -1.62, 0.0), atmosphere: (preset: Airless)),
    }},
)"#
    );
    let level = regia_scene::level::Level::from_ron_str(level_id(), &text).expect("migrates");
    assert_eq!(
        level.scene.format_version,
        regia_scene::format::SCENE_FORMAT_VERSION
    );
    assert_eq!(level.settings.gravity, Vec3::new(0.0, -1.62, 0.0));
    assert_eq!(
        level.settings.atmosphere.preset,
        regia_scene::level::AtmospherePreset::Airless
    );
    assert!(
        level
            .scene
            .resources
            .contains_key(regia_scene::level::LEVEL_SETTINGS_PATH),
        "the migrations found and respelled the settings"
    );
}
