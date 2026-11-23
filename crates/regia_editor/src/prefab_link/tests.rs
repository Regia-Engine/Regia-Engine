// SPDX-License-Identifier: MPL-2.0
//! Break link and file-backed Apply (spec 14 § A3, ADR-7203).

use std::path::{Path, PathBuf};

use regia_app::prelude::App;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_scene::prefab::{PrefabInstance, PrefabLink};
use regia_scene::prelude::{SceneFile, Transform};
use serde_json::json;

use crate::command::{UndoStack, redo_world, undo_world};
use crate::inspector;

const LID: u128 = 0x8b6f_2a11_0000_4000_8000_0000_0000_0002;

fn app_with_project(label: &str) -> (App, PathBuf) {
    let dir = regia_test_utils::unique_temp_dir(label);
    std::fs::create_dir_all(dir.join("assets/props")).expect("dir");
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut().insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    let open = crate::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    app.insert_resource(UndoStack::new());
    (app, dir)
}

/// A prefab with a root and one child ("Lid") one meter up.
fn write_crate(dir: &Path) -> PathBuf {
    let path = dir.join("assets/props/crate.prefab.ron");
    let text = r#"( format_version: 1, entities: [
        ( guid: "8b6f2a11-0000-4000-8000-000000000001", name: "Crate",
          components: { "regia_scene::transform::Transform": () } ),
        ( guid: "8b6f2a11-0000-4000-8000-000000000002", name: "Lid", parent: Some("8b6f2a11-0000-4000-8000-000000000001"),
          components: { "regia_scene::transform::Transform": ( translation: (0.0, 1.0, 0.0) ) } ),
    ] )"#;
    std::fs::write(&path, text).expect("prefab");
    path
}

/// The live entity of `local` inside the instance rooted at `root`.
fn member(world: &mut World, root: Entity, local: u128) -> Entity {
    let mut links = world.query::<(Entity, &PrefabLink)>();
    let candidates: Vec<Entity> = links
        .iter(world)
        .filter(|(_, link)| link.local_id == local)
        .map(|(entity, _)| entity)
        .collect();
    candidates
        .into_iter()
        .find(|entity| {
            let mut cursor = Some(*entity);
            while let Some(at) = cursor {
                if at == root {
                    return true;
                }
                cursor = world.get::<ChildOf>(at).map(ChildOf::parent);
            }
            false
        })
        .expect("member of the instance")
}

fn lid_y(world: &mut World, root: Entity) -> f32 {
    let lid = member(world, root, LID);
    world.get::<Transform>(lid).expect("transform").translation.y
}

fn set_lid_y(world: &mut World, root: Entity, y: f32) {
    let lid = member(world, root, LID);
    inspector::edit_value(
        world,
        &[lid],
        "Transform",
        "translation",
        Box::new(Vec3::new(0.0, y, 0.0)),
    )
    .expect("edit");
}

#[test]
fn break_link_keeps_the_entities_and_values_in_one_undo_step() {
    let (mut app, dir) = app_with_project("prefab-break-link");
    let path = write_crate(&dir);
    let root = crate::content::spawn_prefab_asset(app.world_mut(), &path, Vec3::new(2.0, 0.0, 0.0))
        .expect("instance");
    set_lid_y(app.world_mut(), root, 4.0);
    let lid = member(app.world_mut(), root, LID);
    let depth = app.world().resource::<UndoStack>().undo_depth();

    let report = super::break_prefab_link(app.world_mut(), lid).expect("break link");
    assert_eq!(
        report.root,
        root.to_bits(),
        "a member resolves to its instance root"
    );
    assert_eq!(report.unlinked, 2, "the template root and the lid: {report:?}");
    let world = app.world_mut();
    assert!(world.get::<PrefabInstance>(root).is_none());
    assert!(world.get::<PrefabLink>(lid).is_none());
    assert_eq!(
        world.get::<Transform>(lid).expect("kept").translation,
        Vec3::new(0.0, 4.0, 0.0),
        "the overridden value stays"
    );
    let crate_root = world
        .get::<ChildOf>(lid)
        .map(ChildOf::parent)
        .expect("parent kept");
    assert_eq!(world.get::<ChildOf>(crate_root).map(ChildOf::parent), Some(root));
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth + 1,
        "one undo step"
    );
    // A snapshot writes the former members as plain entities now.
    let open = world.resource::<crate::level_io::OpenLevel>().id;
    let level = regia_scene::level::snapshot_level(world, open).expect("snapshot");
    assert!(level.scene.entities.iter().all(|e| e.prefab_instance.is_none()));
    assert_eq!(
        level.scene.entities.len(),
        3,
        "wrapper, crate and lid are scene entities"
    );

    assert!(undo_world(app.world_mut()));
    assert!(app.world().get::<PrefabInstance>(root).is_some());
    assert_eq!(app.world().get::<PrefabLink>(lid).map(|l| l.local_id), Some(LID));
    assert!(redo_world(app.world_mut()));
    assert!(app.world().get::<PrefabLink>(lid).is_none());

    // Not an instance.
    let plain = app.world_mut().spawn(Transform::default()).id();
    let err = super::break_prefab_link(app.world_mut(), plain).expect_err("plain entity");
    assert_eq!(err.code, "not_prefab_instance");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn break_link_through_the_agent_method() {
    let (mut app, dir) = app_with_project("prefab-break-link-agent");
    crate::agent_ext::install(app.world_mut());
    let path = write_crate(&dir);
    let root = crate::content::spawn_prefab_asset(app.world_mut(), &path, Vec3::ZERO).expect("instance");
    let (_, handler) = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .method("editor.prefab_break_link")
        .expect("registered");
    let reply = handler(app.world_mut(), &json!({ "entity": root.to_bits() })).expect("reply");
    assert_eq!(reply["root"], json!(root.to_bits()));
    assert_eq!(reply["unlinked"], json!(2));
    assert!(app.world().get::<PrefabInstance>(root).is_none());
    let err = handler(app.world_mut(), &json!({ "entity": root.to_bits() })).expect_err("no longer one");
    let reason = err
        .data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(|r| r.as_str());
    assert_eq!(reason, Some("not_prefab_instance"), "{err:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_writes_the_prefab_file_and_other_instances_reload_keeping_their_overrides() {
    let (mut app, dir) = app_with_project("prefab-apply-file");
    let path = write_crate(&dir);
    let before = std::fs::read(&path).expect("read");
    let a = crate::content::spawn_prefab_asset(app.world_mut(), &path, Vec3::ZERO).expect("a");
    let b = crate::content::spawn_prefab_asset(app.world_mut(), &path, Vec3::X).expect("b");
    let c = crate::content::spawn_prefab_asset(app.world_mut(), &path, Vec3::Z).expect("c");
    set_lid_y(app.world_mut(), b, 7.0);
    set_lid_y(app.world_mut(), a, 5.0);
    let lid_a = member(app.world_mut(), a, LID);

    inspector::apply_prefab_override(app.world_mut(), lid_a, "Transform", "translation").expect("apply");

    // The file holds the applied value, in the current format, and round-trips byte for byte.
    let written = std::fs::read_to_string(&path).expect("reread");
    let parsed = SceneFile::from_ron_str(&written).expect("parses");
    assert_eq!(parsed.format_version, regia_scene::format::SCENE_FORMAT_VERSION);
    assert_eq!(
        parsed.to_ron_string().expect("ron"),
        written,
        "canonical bytes round-trip"
    );
    let lid_record = parsed
        .entities
        .iter()
        .find(|e| e.guid == Guid::from_u128(LID))
        .expect("lid record");
    assert!(
        lid_record
            .components
            .values()
            .any(|raw| raw.get_ron().contains("5.0")),
        "{written}"
    );
    assert_eq!(
        lid_record.name.as_deref(),
        Some("Lid"),
        "record names survive the write"
    );
    // No temp file left behind.
    let leftovers: Vec<_> = std::fs::read_dir(dir.join("assets/props"))
        .expect("dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    let world = app.world_mut();
    assert!(world.get::<PrefabInstance>(a).expect("a").overrides.is_empty());
    assert_eq!(lid_y(world, a), 5.0);
    assert_eq!(lid_y(world, b), 7.0, "b keeps its own override");
    assert_eq!(world.get::<PrefabInstance>(b).expect("b").overrides.len(), 1);
    assert_eq!(lid_y(world, c), 5.0, "c reloads from the applied prefab");

    // The watcher sees the editor's own write: nothing respawns, b still keeps its override.
    let lid_b = member(world, b, LID);
    crate::prefab_edit::reload_prefab_file(world, &path).expect("reload");
    assert_eq!(lid_y(world, b), 7.0);
    assert!(
        world.get_entity(lid_b).is_ok(),
        "the echo of our own write respawns nothing"
    );

    // One undo puts the file bytes, a's override and c's value back.
    assert!(undo_world(app.world_mut()));
    assert_eq!(std::fs::read(&path).expect("reread"), before);
    let world = app.world_mut();
    assert_eq!(world.get::<PrefabInstance>(a).expect("a").overrides.len(), 1);
    assert_eq!(lid_y(world, c), 1.0);
    assert_eq!(lid_y(world, b), 7.0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An entity of a prefab nested inside the applied prefab: the value goes into the outer file
/// as an override on the nested instance record (outer wins); the inner file is not touched.
#[test]
fn apply_on_a_nested_member_writes_an_outer_override_and_leaves_the_inner_file() {
    let (mut app, dir) = app_with_project("prefab-apply-nested");
    let inner_path = write_crate(&dir);
    let inner_id = crate::content::prefab_asset_id(&inner_path).expect("meta").0;
    let inner_before = std::fs::read(&inner_path).expect("inner");
    let outer_path = dir.join("assets/props/stack.prefab.ron");
    let outer_text = format!(
        r#"( format_version: 1, entities: [
        ( guid: "6e0c0000-0000-4000-8000-000000000001", name: "Stack",
          components: {{ "regia_scene::transform::Transform": () }} ),
        ( guid: "6e0c0000-0000-4000-8000-000000000002", parent: Some("6e0c0000-0000-4000-8000-000000000001"),
          components: {{ "regia_scene::transform::Transform": () }},
          prefab_instance: ( prefab: "asset:{inner_id}" ) ),
    ] )"#
    );
    std::fs::write(&outer_path, outer_text).expect("outer");
    // The inner template must be registered for the outer one to instantiate.
    crate::prefab_variant_edit::register_prefab_file(app.world_mut(), &inner_path).expect("inner");
    let one = crate::content::spawn_prefab_asset(app.world_mut(), &outer_path, Vec3::ZERO).expect("one");
    let two = crate::content::spawn_prefab_asset(app.world_mut(), &outer_path, Vec3::X).expect("two");
    set_lid_y(app.world_mut(), one, 3.0);
    let lid_one = member(app.world_mut(), one, LID);

    inspector::apply_prefab_override(app.world_mut(), lid_one, "Transform", "translation").expect("apply");

    assert_eq!(
        std::fs::read(&inner_path).expect("inner"),
        inner_before,
        "inner file untouched"
    );
    let outer =
        SceneFile::from_ron_str(&std::fs::read_to_string(&outer_path).expect("outer")).expect("parse");
    let nested = outer
        .entities
        .iter()
        .find_map(|e| e.prefab_instance.as_ref())
        .expect("nested record");
    assert_eq!(nested.overrides.len(), 1, "{:?}", nested.overrides);
    assert_eq!(nested.overrides[0].target, Guid::from_u128(LID));
    assert!(nested.overrides[0].value.get_ron().contains("3.0"));
    assert_eq!(
        lid_y(app.world_mut(), two),
        3.0,
        "the other outer instance reloads"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec 14 § A3 (ADR-7263): Apply on an instance of a prefab variant writes the override into the
/// `.variant.ron` (atomic, current format) instead of refusing; the other instances of the
/// variant take the value, the base file and the base's own instances are untouched, and one
/// undo puts the variant bytes, the override and the values back.
#[test]
fn apply_on_a_variant_instance_writes_the_variant_file_and_leaves_the_base() {
    use regia_scene::prefab_variant::{PREFAB_VARIANT_FORMAT_VERSION, PrefabVariantFile};
    let (mut app, dir) = app_with_project("prefab-apply-variant");
    let base = write_crate(&dir);
    let variant = dir.join("assets/props/crate_tall.variant.ron");
    crate::prefab_variant_edit::create_prefab_variant(app.world_mut(), &base, &variant, None)
        .expect("variant");
    let base_before = std::fs::read(&base).expect("base");
    let variant_before = std::fs::read(&variant).expect("variant");
    let a = crate::content::spawn_prefab_asset(app.world_mut(), &variant, Vec3::ZERO).expect("a");
    let b = crate::content::spawn_prefab_asset(app.world_mut(), &variant, Vec3::X).expect("b");
    let plain = crate::content::spawn_prefab_asset(app.world_mut(), &base, Vec3::Z).expect("plain");
    set_lid_y(app.world_mut(), a, 5.0);
    let lid_a = member(app.world_mut(), a, LID);

    inspector::apply_prefab_override(app.world_mut(), lid_a, "Transform", "translation")
        .expect("apply on a variant instance");

    // The variant file holds the override, in the current format, and round-trips byte for byte.
    let written = std::fs::read_to_string(&variant).expect("reread");
    let parsed = PrefabVariantFile::from_ron_str(&written).expect("parses");
    assert_eq!(parsed.format_version, PREFAB_VARIANT_FORMAT_VERSION);
    assert_eq!(
        parsed.to_ron_string().expect("ron"),
        written,
        "canonical bytes round-trip"
    );
    let entry = parsed
        .base
        .overrides
        .iter()
        .find(|o| o.target == Guid::from_u128(LID))
        .expect("the override is in the variant file");
    assert!(entry.component.ends_with("Transform"), "{written}");
    assert!(entry.value.get_ron().contains("5.0"), "{written}");
    assert_ne!(written.as_bytes(), variant_before.as_slice());
    assert_eq!(
        std::fs::read(&base).expect("base"),
        base_before,
        "the base file is untouched"
    );
    let leftovers: Vec<_> = std::fs::read_dir(dir.join("assets/props"))
        .expect("dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    let world = app.world_mut();
    assert!(world.get::<PrefabInstance>(a).expect("a").overrides.is_empty());
    assert_eq!(lid_y(world, a), 5.0);
    assert_eq!(
        lid_y(world, b),
        5.0,
        "a second variant instance shows the override"
    );
    assert_eq!(
        lid_y(world, plain),
        1.0,
        "an instance of the base keeps the base value"
    );

    // One undo puts the variant bytes, a's override and b's value back.
    assert!(undo_world(app.world_mut()));
    assert_eq!(std::fs::read(&variant).expect("reread"), variant_before);
    let world = app.world_mut();
    assert_eq!(world.get::<PrefabInstance>(a).expect("a").overrides.len(), 1);
    assert_eq!(lid_y(world, a), 5.0);
    assert_eq!(lid_y(world, b), 1.0);
    assert!(redo_world(app.world_mut()));
    assert_eq!(std::fs::read_to_string(&variant).expect("reread"), written);
    assert_eq!(lid_y(app.world_mut(), b), 5.0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The agent method `editor.prefab_apply_override` applies on a variant instance as on a plain
/// one, and names the file it wrote.
#[test]
fn apply_override_through_the_agent_method_accepts_variants() {
    let (mut app, dir) = app_with_project("prefab-apply-variant-agent");
    crate::agent_ext::install(app.world_mut());
    let base = write_crate(&dir);
    let variant = dir.join("assets/props/crate_tall.variant.ron");
    crate::prefab_variant_edit::create_prefab_variant(app.world_mut(), &base, &variant, None)
        .expect("variant");
    let a = crate::content::spawn_prefab_asset(app.world_mut(), &variant, Vec3::ZERO).expect("a");
    let b = crate::content::spawn_prefab_asset(app.world_mut(), &variant, Vec3::X).expect("b");
    set_lid_y(app.world_mut(), a, 3.0);
    let lid_a = member(app.world_mut(), a, LID);
    let (_, handler) = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .method("editor.prefab_apply_override")
        .expect("registered");
    let reply = handler(
        app.world_mut(),
        &json!({ "entity": lid_a.to_bits(), "component": "Transform", "field": "translation" }),
    )
    .expect("reply");
    assert_eq!(
        reply["file"],
        json!("assets/props/crate_tall.variant.ron"),
        "{reply}"
    );
    assert_eq!(reply["edited"], json!([lid_a.to_bits()]));
    assert_eq!(lid_y(app.world_mut(), b), 3.0);
    let err = handler(
        app.world_mut(),
        &json!({ "entity": lid_a.to_bits(), "component": "Transform", "field": "translation" }),
    )
    .expect_err("the override is gone");
    let reason = err
        .data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(|r| r.as_str());
    assert_eq!(reason, Some("override_absent"), "{err:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
