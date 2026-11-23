// SPDX-License-Identifier: MPL-2.0
//! Create prefab from selection (spec 19 § 2 Outliner, ADR-7468): the acceptance tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_app::prelude::App;
use regia_core::id::Guid;
use regia_core::math::{Quat, Vec3};
use regia_scene::format::SceneEntity;
use regia_scene::prefab::{PrefabInstance, PrefabLibrary, PrefabTemplate};
use regia_scene::prelude::{Name, SceneFile, SpawnOptions, Transform};
use serde_json::json;

use super::*;
use crate::command::{SpawnEntities, UndoStack, redo_world, undo_world};
use crate::level_io::{self, OpenLevel};

const CRATE: u128 = 0x5a11_0000_0000_4000_8000_0000_0000_0001;
const LID: u128 = 0x5a11_0000_0000_4000_8000_0000_0000_0002;
const BARREL: u128 = 0x5a11_0000_0000_4000_8000_0000_0000_0003;
const SHED: u128 = 0x5a11_0000_0000_4000_8000_0000_0000_0004;

fn app_with_project(label: &str) -> (App, PathBuf) {
    let dir = regia_test_utils::unique_temp_dir(label);
    std::fs::create_dir_all(dir.join("assets")).expect("dir");
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut().insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    app.insert_resource(UndoStack::new());
    (app, dir)
}

fn raw_transform(world: &World, transform: Transform) -> Box<ron::value::RawValue> {
    super::transform_raw(world, &transform).expect("transform")
}

fn record(world: &World, guid: u128, name: &str, parent: Option<u128>, transform: Transform) -> SceneEntity {
    SceneEntity {
        guid: Guid::from_u128(guid),
        name: Some(name.to_string()),
        parent: parent.map(Guid::from_u128),
        components: BTreeMap::from([(TRANSFORM_KEY.to_string(), raw_transform(world, transform))]),
        prefab_instance: None,
    }
}

/// The level: Shed (a root, moved and turned) holding Crate (with a child Lid one meter up) and
/// Barrel; every entity a level member.
fn build_level(world: &mut World) {
    let mut file = SceneFile::new();
    file.entities = vec![
        record(
            world,
            SHED,
            "Shed",
            None,
            Transform {
                translation: Vec3::new(10.0, 0.0, -4.0),
                rotation: Quat::from_rotation_y(0.5),
                ..Transform::default()
            },
        ),
        record(
            world,
            CRATE,
            "Crate",
            Some(SHED),
            Transform {
                translation: Vec3::new(2.0, 0.0, 1.0),
                rotation: Quat::from_rotation_y(0.25),
                scale: Vec3::splat(1.5),
            },
        ),
        record(
            world,
            LID,
            "Lid",
            Some(CRATE),
            Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)),
        ),
        record(
            world,
            BARREL,
            "Barrel",
            Some(SHED),
            Transform::from_translation(Vec3::new(-3.0, 0.0, 2.0)),
        ),
    ];
    let mut spawn = SpawnEntities {
        file,
        spawned_guids: Vec::new(),
        spawned_entities: Vec::new(),
    };
    spawn.apply(world);
    crate::visibility::ensure_scene_identity(world);
}

fn by_guid(world: &World, guid: u128) -> Entity {
    hierarchy_ops::guid_entity(world, Guid::from_u128(guid)).expect("entity")
}

fn by_name(world: &mut World, name: &str) -> Vec<Entity> {
    let mut query = world.query::<(Entity, &Name)>();
    query
        .iter(world)
        .filter(|(_, n)| n.as_str() == name)
        .map(|(e, _)| e)
        .collect()
}

fn world_position(world: &World, entity: Entity) -> Vec3 {
    Vec3::from(hierarchy_ops::world_affine(world, entity).translation)
}

fn level_bytes(world: &mut World, dir: &Path, label: &str) -> String {
    let open = world.resource::<OpenLevel>().clone();
    let path = dir.join(format!("assets/{label}.level.ron"));
    level_io::save_level_file(world, &open, &path).expect("save level");
    std::fs::read_to_string(&path).expect("level text")
}

/// The written prefab parses, is at the current format version, and loads back through the
/// runtime path (parse, bake, `spawn_prefab` in a fresh world) with the selection's entities.
#[test]
fn created_prefab_loads_back_through_the_runtime_loader() {
    let (mut app, dir) = app_with_project("prefab-create-runtime");
    let world = app.world_mut();
    build_level(world);
    let crate_entity = by_guid(world, CRATE);
    let path = dir.join("assets/props/crate.prefab.ron");
    let created = create_prefab(world, &[crate_entity], &path).expect("created");
    assert_eq!(created.entities, 2, "Crate and Lid");
    assert!(regia_asset::meta_path_for(&path).is_file(), "the .meta exists");

    let text = std::fs::read_to_string(&path).expect("prefab text");
    let file = SceneFile::from_ron_str(&text).expect("parses");
    assert_eq!(file.format_version, regia_scene::format::SCENE_FORMAT_VERSION);
    let roots: Vec<&SceneEntity> = file.entities.iter().filter(|e| e.parent.is_none()).collect();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].name.as_deref(), Some("Crate"));

    // A fresh runtime world: no editor resources, only the scene plugin.
    let mut runtime = App::new();
    runtime.add_plugins(regia_scene::ScenePlugin);
    let registry = runtime.world().resource::<AppTypeRegistry>().clone();
    let asset = crate::content::prefab_asset_id(&path).expect("meta id").0;
    assert_eq!(asset, created.asset);
    let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
    assert!(template.warnings.is_empty(), "{:?}", template.warnings);
    runtime
        .world_mut()
        .resource_mut::<PrefabLibrary>()
        .insert(template);
    let root = runtime
        .world_mut()
        .spawn_prefab(
            asset,
            Some(Transform::default()),
            &SpawnOptions::default(),
            Guid::new_random(),
        )
        .expect("runtime spawn");
    assert!(runtime.world().get::<PrefabInstance>(root).is_some());
    let mut names: Vec<String> = runtime
        .world_mut()
        .query::<&Name>()
        .iter(runtime.world())
        .map(|n| n.as_str().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["Crate".to_string(), "Lid".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review sprint-w12-rev-early-k row 11: the Outliner folder of the entity a prefab is made from
/// stays in the level; the `.prefab.ron` has no `EditorFolder`, so placements land at the root.
#[test]
fn a_prefab_from_a_foldered_entity_carries_no_folder() {
    let (mut app, dir) = app_with_project("prefab-create-folder");
    let world = app.world_mut();
    build_level(world);
    let crate_entity = by_guid(world, CRATE);
    world
        .entity_mut(crate_entity)
        .insert(regia_scene::components::EditorFolder {
            path: "Lights".to_string(),
        });
    let path = dir.join("assets/props/crate_folder.prefab.ron");
    create_prefab(world, &[crate_entity], &path).expect("created");
    let text = std::fs::read_to_string(&path).expect("prefab text");
    assert!(!text.contains("EditorFolder"), "{text}");
    let file = SceneFile::from_ron_str(&text).expect("parses");
    assert!(
        file.entities
            .iter()
            .all(|e| !e.components.contains_key(EDITOR_FOLDER_KEY)),
        "no record keeps the folder"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The selection becomes one linked instance at the same world transforms; the level re-saves
/// with the instance link and without the original records; undo restores the level bytes
/// exactly, redo brings the instance back, and undo is exact again.
#[test]
fn level_resaves_with_the_link_and_undo_restores_its_bytes() {
    let (mut app, dir) = app_with_project("prefab-create-level");
    let world = app.world_mut();
    build_level(world);
    let crate_entity = by_guid(world, CRATE);
    let lid_before = world_position(world, by_guid(world, LID));
    let crate_before = hierarchy_ops::world_affine(world, crate_entity);
    let before = level_bytes(world, &dir, "before");
    let depth_before = world.resource::<UndoStack>().undo_depth();

    let path = dir.join("assets/props/crate.prefab.ron");
    let created = create_prefab(world, &[crate_entity], &path).expect("created");
    assert_eq!(created.undo_depth, depth_before + 1, "one undo step");
    assert!(world.get_entity(crate_entity).is_err(), "the original is gone");
    let instance = created.instance;
    assert_eq!(
        world
            .get::<PrefabInstance>(instance)
            .map(PrefabInstance::prefab_guid),
        Some(created.asset)
    );
    assert_eq!(
        world.get::<ChildOf>(instance).map(ChildOf::parent),
        Some(by_guid(world, SHED)),
        "the instance takes the original's place under Shed"
    );
    assert_eq!(
        world.resource::<crate::selection::Selection>().entities(),
        &[instance],
        "the instance is the selection"
    );
    // Same world transforms: the instance's Crate and Lid sit where the originals did.
    let crates = by_name(world, "Crate");
    assert_eq!(crates.len(), 1);
    let crate_after = hierarchy_ops::world_affine(world, crates[0]);
    assert!(
        crate_after.abs_diff_eq(crate_before, 1e-5),
        "{crate_after:?} vs {crate_before:?}"
    );
    let lids = by_name(world, "Lid");
    assert_eq!(lids.len(), 1);
    assert!(world_position(world, lids[0]).abs_diff_eq(lid_before, 1e-5));

    let after = level_bytes(world, &dir, "after");
    let saved = SceneFile::from_ron_str(&after).expect("level parses");
    let link = saved
        .entities
        .iter()
        .find(|e| e.prefab_instance.is_some())
        .expect("an instance record");
    assert_eq!(
        link.prefab_instance.as_ref().map(|p| p.prefab.guid()),
        Some(created.asset)
    );
    assert_eq!(link.parent, Some(Guid::from_u128(SHED)));
    for gone in [CRATE, LID] {
        assert!(
            saved.entities.iter().all(|e| e.guid != Guid::from_u128(gone)),
            "{gone:x} is inside the prefab now"
        );
    }

    assert!(undo_world(world));
    assert_eq!(
        level_bytes(world, &dir, "undone"),
        before,
        "undo restores the level bytes"
    );
    assert!(redo_world(world));
    let redone = level_bytes(world, &dir, "redone");
    assert_eq!(redone, after, "redo brings the same instance back");
    assert!(undo_world(world));
    assert_eq!(level_bytes(world, &dir, "undone2"), before);
    assert!(path.is_file(), "the prefab file stays after an undo");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Several top-level entities: a new root named after the file, the instance at their
/// centroid under their common parent, every entity at its old world position.
#[test]
fn several_entities_get_a_new_root_at_their_centroid() {
    let (mut app, dir) = app_with_project("prefab-create-several");
    let world = app.world_mut();
    build_level(world);
    let crate_entity = by_guid(world, CRATE);
    let barrel = by_guid(world, BARREL);
    let lid = by_guid(world, LID);
    let positions: Vec<(&str, Vec3)> = [("Crate", crate_entity), ("Lid", lid), ("Barrel", barrel)]
        .into_iter()
        .map(|(name, e)| (name, world_position(world, e)))
        .collect();
    let centroid = (positions[0].1 + positions[2].1) / 2.0;
    let path = dir.join("assets/props/yard_set.prefab.ron");
    // Lid is under Crate: it travels with it and is not a second member.
    let created = create_prefab(world, &[crate_entity, lid, barrel], &path).expect("created");
    assert_eq!(created.replaced, vec![crate_entity, barrel]);
    assert_eq!(created.entities, 4, "new root + Crate + Lid + Barrel");
    let file = SceneFile::from_ron_str(&std::fs::read_to_string(&path).expect("text")).expect("parses");
    let root = file.entities.iter().find(|e| e.parent.is_none()).expect("root");
    assert_eq!(root.name.as_deref(), Some("yard_set"));
    let instance = created.instance;
    assert_eq!(
        world.get::<ChildOf>(instance).map(ChildOf::parent),
        Some(by_guid(world, SHED))
    );
    assert!(world_position(world, instance).abs_diff_eq(centroid, 1e-4));
    for (name, before) in positions {
        let found = by_name(world, name);
        assert_eq!(found.len(), 1, "{name}");
        let after = world_position(world, found[0]);
        assert!(after.abs_diff_eq(before, 1e-4), "{name}: {after} vs {before}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Refusals change nothing: a bad name, an existing file, a prefab-internal entity, no
/// entities, prefab edit mode.
#[test]
fn refusals_write_nothing() {
    let (mut app, dir) = app_with_project("prefab-create-refuse");
    let world = app.world_mut();
    build_level(world);
    let crate_entity = by_guid(world, CRATE);
    let before = level_bytes(world, &dir, "before");
    let code = |result: Result<CreatedPrefab, CreatePrefabError>| result.expect_err("refused").code();

    assert_eq!(
        code(create_prefab(
            world,
            &[crate_entity],
            &dir.join("assets/crate.ron")
        )),
        "prefab_create_bad_path"
    );
    assert_eq!(
        code(create_prefab(
            world,
            &[crate_entity],
            &dir.join("assets/crate.variant.prefab.ron")
        )),
        "prefab_create_bad_path"
    );
    let taken = dir.join("assets/taken.prefab.ron");
    std::fs::write(&taken, "()").expect("taken");
    assert_eq!(
        code(create_prefab(world, &[crate_entity], &taken)),
        "prefab_create_exists"
    );
    assert_eq!(
        code(create_prefab(world, &[], &dir.join("assets/none.prefab.ron"))),
        "hierarchy_no_entities"
    );

    // An entity inside another instance.
    let first = dir.join("assets/first.prefab.ron");
    let made = create_prefab(world, &[crate_entity], &first).expect("first");
    let member = by_name(world, "Lid")[0];
    assert!(world.get::<PrefabLink>(member).is_some());
    assert_eq!(
        code(create_prefab(
            world,
            &[member],
            &dir.join("assets/inner.prefab.ron")
        )),
        "prefab_create_internal"
    );
    // The instance root itself nests fine.
    let nested = dir.join("assets/nested.prefab.ron");
    create_prefab(world, &[made.instance], &nested).expect("nested prefab");
    let text = std::fs::read_to_string(&nested).expect("text");
    assert!(
        text.contains("prefab_instance"),
        "the inner instance stays an instance: {text}"
    );
    assert!(undo_world(world) && undo_world(world));
    assert_eq!(level_bytes(world, &dir, "undone"), before);
    for name in ["crate.ron", "none.prefab.ron", "inner.prefab.ron"] {
        assert!(!dir.join("assets").join(name).exists(), "{name} written");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `editor.create_prefab` through the registered agent extension (the dispatcher's table):
/// the selection default, the reply, and the confined-path refusal.
#[test]
fn create_prefab_through_the_agent_method() {
    let (mut app, dir) = app_with_project("prefab-create-agent");
    crate::agent_ext::install(app.world_mut());
    build_level(app.world_mut());
    let crate_entity = by_guid(app.world(), CRATE);
    let (_, handler) = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .method("editor.create_prefab")
        .expect("registered");

    // Outside the project: the confined-path code, nothing written.
    for outside in ["../escape.prefab.ron", "/tmp/escape.prefab.ron"] {
        let err = handler(
            app.world_mut(),
            &json!({ "entities": [crate_entity.to_bits()], "path": outside }),
        )
        .expect_err("outside the project");
        let reason = err
            .data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(|r| r.as_str());
        assert_eq!(reason, Some("path_outside_project"), "{outside}: {err:?}");
    }
    assert!(!dir.join("escape.prefab.ron").exists());
    assert!(app.world().get_entity(crate_entity).is_ok(), "nothing replaced");

    let err = handler(app.world_mut(), &json!({ "path": "props/a.prefab.ron" })).expect_err("empty");
    assert_eq!(
        err.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(|r| r.as_str()),
        Some("hierarchy_no_entities")
    );

    app.world_mut()
        .resource_mut::<crate::selection::Selection>()
        .select(crate_entity, crate::selection::SelectMode::Replace);
    let reply = handler(app.world_mut(), &json!({ "path": "props/crate.prefab.ron" })).expect("reply");
    assert_eq!(reply["path"], "assets/props/crate.prefab.ron");
    assert_eq!(reply["entities"], 2);
    assert_eq!(reply["replaced"], json!([crate_entity.to_bits()]));
    let instance = crate::command::entity_from_bits(reply["instance"].as_u64().expect("id")).expect("entity");
    assert!(app.world().get::<PrefabInstance>(instance).is_some());
    assert!(dir.join("assets/props/crate.prefab.ron").is_file());
    let err = handler(app.world_mut(), &json!({ "path": "props/crate.prefab.ron" })).expect_err("exists");
    assert_eq!(
        err.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(|r| r.as_str()),
        Some("prefab_create_exists")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn default_path_and_stem() {
    let mut world = World::new();
    let e = world.spawn(Name::new("Red Crate #2")).id();
    assert_eq!(
        default_prefab_path(&world, &[e]),
        "assets/prefabs/red_crate__2.prefab.ron"
    );
    assert_eq!(
        default_prefab_path(&world, &[]),
        "assets/prefabs/new_prefab.prefab.ron"
    );
    assert_eq!(
        prefab_stem(Path::new("a/b/crate.prefab.ron")).expect("stem"),
        "crate"
    );
    assert!(prefab_stem(Path::new("a/.prefab.ron")).is_err());
}

/// A component that names another entity (like a joint's `connected` or a chase camera's
/// `target`; the Inspector's entity eyedropper writes such fields).
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct Aim {
    target: Option<Entity>,
}

fn aim(world: &World, entity: Entity) -> Option<Entity> {
    world.get::<Aim>(entity).and_then(|aim| aim.target)
}

/// The prefab-internal entity of `instance` made from the member whose guid was `guid`.
fn internal(world: &mut World, instance_guid: Guid, guid: u128) -> Entity {
    let instance = hierarchy_ops::guid_entity(world, instance_guid).expect("instance");
    crate::outliner::subtree_of(world, instance)
        .into_iter()
        .find(|e| {
            world
                .get::<regia_scene::prefab::PrefabLink>(*e)
                .is_some_and(|link| link.local_id == guid)
        })
        .expect("internal entity")
}

/// Lane v06-editor-usability-a item 1 (w11 "Create prefab leaves outside references dangling"):
/// an entity outside the selection that names a member, a member that names another member and
/// a member that names an outside entity all name the right live entity after Create prefab, after
/// its undo (the members are back, as new entities) and after the redo (a new instance). Before
/// the fix the outside reference and the member-to-member reference named despawned entities.
#[test]
fn entity_references_follow_the_members_through_create_undo_and_redo() {
    let (mut app, dir) = app_with_project("prefab-create-refs");
    app.register_type::<Aim>();
    let world = app.world_mut();
    let turret = world
        .spawn((
            EntityGuid(Guid::from_u128(0x7001)),
            Name::new("Turret"),
            Transform::default(),
        ))
        .id();
    let body = world
        .spawn((
            EntityGuid(Guid::from_u128(CRATE)),
            Name::new("Body"),
            Transform::default(),
        ))
        .id();
    let arm = world
        .spawn((
            EntityGuid(Guid::from_u128(LID)),
            Name::new("Arm"),
            Transform::default(),
            Aim { target: Some(turret) },
            ChildOf(body),
        ))
        .id();
    world.entity_mut(body).insert(Aim { target: Some(arm) });
    world.entity_mut(turret).insert(Aim { target: Some(body) });

    let created = create_prefab(world, &[body], &dir.join("assets/robot.prefab.ron")).expect("created");
    let instance_guid = world
        .get::<EntityGuid>(created.instance)
        .expect("instance guid")
        .0;
    let check_instance = |world: &mut World, what: &str| {
        let new_body = internal(world, instance_guid, CRATE);
        let new_arm = internal(world, instance_guid, LID);
        let turret = by_guid(world, 0x7001);
        assert_eq!(aim(world, turret), Some(new_body), "{what}: outside -> member");
        assert_eq!(aim(world, new_body), Some(new_arm), "{what}: member -> member");
        assert_eq!(aim(world, new_arm), Some(turret), "{what}: member -> outside");
    };
    check_instance(world, "create");

    assert!(undo_world(world), "undo");
    let (body, arm, turret) = (by_guid(world, CRATE), by_guid(world, LID), by_guid(world, 0x7001));
    assert_eq!(aim(world, turret), Some(body), "undo: outside -> member");
    assert_eq!(aim(world, body), Some(arm), "undo: member -> member");
    assert_eq!(aim(world, arm), Some(turret), "undo: member -> outside");

    assert!(redo_world(world), "redo");
    check_instance(world, "redo");
    let _ = std::fs::remove_dir_all(dir);
}
