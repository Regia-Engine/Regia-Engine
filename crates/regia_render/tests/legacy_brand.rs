// SPDX-License-Identifier: MPL-2.0
//! Levels, prefabs and cooked scenes written by the v0.1.29 engine load (ADR-1388, ADR-1429).
//!
//! The fixtures were written by the v0.1.29 engine's own code (a scratch test in a v0.1.29
//! checkout: the published level and prefab examples spawned, snapshotted and cooked, and a level
//! with render components), kept byte for byte. Every component key names the legacy crates.

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::components::{Name, Tags};
use regia_scene::cooked::CookedScene;
use regia_scene::format::SceneFile;
use regia_scene::prefab::{PrefabLibrary, PrefabTemplate};
use regia_scene::spawn::{SpawnOptions, SpawnedScene, spawn_scene};
use regia_scene::transform::Transform;

const COURTYARD: &str = include_str!("fixtures/legacy-brand/courtyard.level.ron");
const LAMP: &str = include_str!("fixtures/legacy-brand/lamp.prefab.ron");
const LIGHTS: &str = include_str!("fixtures/legacy-brand/lights.level.ron");
const COURTYARD_COOKED: &[u8] = include_bytes!("fixtures/legacy-brand/courtyard.cooked_scene.bin");
const LIGHTS_COOKED: &[u8] = include_bytes!("fixtures/legacy-brand/lights.cooked_scene.bin");

/// The lamp prefab's asset id, as the courtyard level references it.
const LAMP_ASSET: &str = "44fe6f0a-1e42-4c11-8d5a-9b3c2f7e10d0";

fn legacy_app() -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    regia_render::register_render_types(&mut app);
    let registry = app.world().resource::<AppTypeRegistry>().0.clone();
    let lamp = SceneFile::from_ron_str(LAMP).expect("the v0.1.29 prefab parses");
    let asset: Guid = LAMP_ASSET.parse().expect("guid");
    let template = PrefabTemplate::from_scene_file(asset, &lamp, &registry.read()).expect("bake");
    // The published example names `PointLight` by a short path that never was a type path (the
    // v0.1.29 bake warned the same); nothing else may be unknown.
    assert!(
        template.warnings.iter().all(|w| w.contains("::PointLight`")),
        "{:?}",
        template.warnings
    );
    app.world_mut().resource_mut::<PrefabLibrary>().insert(template);
    app
}

fn named(world: &mut World, name: &str) -> Entity {
    let mut query = world.query::<(Entity, &Name)>();
    query
        .iter(world)
        .find(|(_, n)| n.0.as_str() == name)
        .map(|(entity, _)| entity)
        .unwrap_or_else(|| panic!("no entity named {name}"))
}

/// The courtyard checks shared by the text and the cooked level: what the v0.1.29 engine itself
/// spawns from this file (checked with a scratch test in a v0.1.29 checkout).
fn check_courtyard(world: &mut World, spawned: &SpawnedScene) {
    // Only the example's `PointLight` short path is unknown, as it was for v0.1.29 (the prefab's
    // light component and the instance's override of it).
    assert!(!spawned.warnings.is_empty());
    assert!(
        spawned.warnings.iter().all(|w| w.contains("PointLight")),
        "{:?}",
        spawned.warnings
    );
    for name in ["Courtyard", "Ground", "North Wall", "Door Assembly", "Door"] {
        let entity = named(world, name);
        assert!(
            world.get::<Transform>(entity).is_some(),
            "{name}: Transform from the legacy key"
        );
    }
    let ground = named(world, "Ground");
    assert!(world.get::<Tags>(ground).is_some(), "Tags from the legacy key");
    named(world, "Yard Lamp");
}

/// The render-component checks shared by the text and the cooked level.
fn check_lights(world: &mut World, spawned: &SpawnedScene) {
    assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);
    let lamp = named(world, "Lamp");
    let light = world
        .get::<regia_render::PointLight>(lamp)
        .expect("PointLight from the legacy key");
    assert_eq!(light.intensity, 321.0);
    let sun = named(world, "Sun");
    assert!(world.get::<regia_render::DirectionalLight>(sun).is_some());
    let crate_entity = named(world, "Crate");
    let mesh = world
        .get::<regia_render::MeshSource>(crate_entity)
        .expect("MeshSource");
    assert_eq!(mesh.mesh, regia_render::MeshSource::BUILTIN_CUBE);
}

#[test]
fn the_fixtures_name_the_legacy_crates() {
    let legacy = regia_core::brand::LEGACY_CRATE_PREFIX;
    assert!(COURTYARD.contains(&format!("\"{legacy}scene::transform::Transform\"")));
    assert!(COURTYARD.contains(&format!("component: \"{legacy}scene::components::Tags\"")));
    assert!(LIGHTS.contains(&format!("\"{legacy}render::lights::PointLight\"")));
}

#[test]
fn a_level_with_a_prefab_instance_written_by_v0129_spawns() {
    let mut app = legacy_app();
    let file = SceneFile::from_ron_str(COURTYARD).expect("parses");
    let spawned = spawn_scene(app.world_mut(), &file, &SpawnOptions::default()).expect("spawn");
    check_courtyard(app.world_mut(), &spawned);
}

#[test]
fn a_level_with_render_components_written_by_v0129_spawns() {
    let mut app = legacy_app();
    let file = SceneFile::from_ron_str(LIGHTS).expect("parses");
    let spawned = spawn_scene(app.world_mut(), &file, &SpawnOptions::default()).expect("spawn");
    check_lights(app.world_mut(), &spawned);
}

#[test]
fn cooked_scenes_written_by_v0129_spawn() {
    let mut app = legacy_app();
    let cooked = CookedScene::from_bytes(LIGHTS_COOKED).expect("decodes");
    let spawned = cooked
        .spawn(app.world_mut(), &SpawnOptions::default())
        .expect("spawn");
    check_lights(app.world_mut(), &spawned);

    let mut app = legacy_app();
    let cooked = CookedScene::from_bytes(COURTYARD_COOKED).expect("decodes");
    let spawned = cooked
        .spawn(app.world_mut(), &SpawnOptions::default())
        .expect("spawn");
    check_courtyard(app.world_mut(), &spawned);
}
