// SPDX-License-Identifier: MPL-2.0
use regia_app::prelude::App;
use regia_scene::prelude::{PrefabLink, Tags};
use regia_scene::transform::Transform;

use super::*;

const ROOT: &str = "8b6f2a11-0000-4000-8000-000000000001";
const CHILD: &str = "8b6f2a11-0000-4000-8000-000000000002";
const TAGS: &str = "regia_scene::components::Tags";

fn project(tag: &str) -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(dir.join("assets/props/deep")).expect("dirs");
    dir
}

fn app(dir: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut().insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.to_path_buf()),
    });
    app
}

/// A prefab: root with a child, the child at height `y`.
fn write_lamp(dir: &Path, y: f32) -> PathBuf {
    let path = dir.join("assets/props/deep/lamp.prefab.ron");
    let text = format!(
        r#"( format_version: 1, entities: [
            ( guid: "{ROOT}", name: "Root", components: {{ "regia_scene::transform::Transform": () }} ),
            ( guid: "{CHILD}", name: "Child", parent: Some("{ROOT}"),
              components: {{ "regia_scene::transform::Transform": ( translation: (0.0, {y:?}, 0.0) ) }} ),
        ] )"#
    );
    regia_test_utils::write_atomic(&path, text.as_bytes()).expect("prefab");
    path
}

fn child_of_instance(world: &mut World, root: Entity) -> Entity {
    let child_id = CHILD.parse::<Guid>().expect("guid").as_u128();
    let mut query = world.query::<(Entity, &PrefabLink, &ChildOf)>();
    let links: Vec<(Entity, Entity)> = query
        .iter(world)
        .filter(|(_, link, _)| link.local_id == child_id)
        .map(|(entity, _, parent)| (entity, parent.parent()))
        .collect();
    // The child hangs under the prefab root, which hangs under the instance root.
    links
        .into_iter()
        .find(|(_, parent)| world.get::<ChildOf>(*parent).is_some_and(|p| p.parent() == root))
        .map(|(entity, _)| entity)
        .expect("child of this instance")
}

fn instances_of(world: &mut World, asset: Guid) -> Vec<Entity> {
    let mut query = world.query::<(Entity, &PrefabInstance)>();
    let mut out: Vec<Entity> = query
        .iter(world)
        .filter(|(_, instance)| instance.prefab_guid() == asset)
        .map(|(entity, _)| entity)
        .collect();
    out.sort();
    out
}

#[test]
fn create_spawn_edit_and_follow_the_base() {
    let dir = project("variant-edit");
    let mut app = app(&dir);
    let world = app.world_mut();
    let base = write_lamp(&dir, 1.0);
    let variant = dir.join("assets/props/red_lamp.variant.ron");

    let asset = create_prefab_variant(world, &base, &variant, None).expect("create");
    let meta = std::fs::read(regia_asset::meta_path_for(&variant)).expect("meta");
    let meta = regia_asset::MetaSidecar::from_ron(&meta).expect("meta parses");
    assert_eq!(meta.id.0, asset);
    assert!(world.resource::<PrefabLibrary>().variant(asset).is_some());
    assert_eq!(
        create_prefab_variant(world, &base, &variant, None).map_err(|e| e.reason),
        Err("variant_exists")
    );

    let spawned =
        crate::content::spawn_prefab_asset(world, &variant, regia_core::math::Vec3::ZERO).expect("spawn");
    assert_eq!(instances_of(world, asset), vec![spawned]);
    let child = child_of_instance(world, spawned);
    assert_eq!(world.get::<Transform>(child).expect("t").translation.y, 1.0);

    // An override on the variant updates the live instance.
    let updated = set_variant_override(
        world,
        &variant,
        CHILD.parse().expect("guid"),
        TAGS,
        ".",
        Some(r#"(["red"])"#),
    )
    .expect("set");
    assert_eq!(updated, 1);
    let instance = instances_of(world, asset)[0];
    let child = child_of_instance(world, instance);
    assert!(world.get::<Tags>(child).expect("tags").contains("red"));
    assert_eq!(read_variant_file(&variant).expect("read").base.overrides.len(), 1);

    // The base changes on disk: reloading it updates the variant's instance too.
    write_lamp(&dir, 3.0);
    let reloaded = crate::prefab_edit::reload_prefab_file(world, &base).expect("reload");
    assert_eq!(reloaded, 1, "the variant's instance updated");
    let instance = instances_of(world, asset)[0];
    let child = child_of_instance(world, instance);
    assert_eq!(world.get::<Transform>(child).expect("t").translation.y, 3.0);
    assert!(world.get::<Tags>(child).expect("tags kept").contains("red"));

    // Clearing the override.
    set_variant_override(world, &variant, CHILD.parse().expect("guid"), TAGS, ".", None).expect("clear");
    assert!(
        read_variant_file(&variant)
            .expect("read")
            .base
            .overrides
            .is_empty()
    );
    let instance = instances_of(world, asset)[0];
    let child = child_of_instance(world, instance);
    assert!(world.get::<Tags>(child).is_none_or(|tags| !tags.contains("red")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_instance_saved_as_a_variant_keeps_its_overrides() {
    let dir = project("variant-from-instance");
    let mut app = app(&dir);
    let world = app.world_mut();
    let base = write_lamp(&dir, 1.0);
    let instance =
        crate::content::spawn_prefab_asset(world, &base, regia_core::math::Vec3::ZERO).expect("spawn");
    world
        .get_mut::<PrefabInstance>(instance)
        .expect("record")
        .overrides
        .push(regia_scene::prefab::OverrideEntry {
            target: CHILD.parse::<Guid>().expect("guid").as_u128(),
            component: TAGS.into(),
            path: ".".into(),
            value: r#"(["mossy"])"#.into(),
        });
    let variant = dir.join("assets/mossy.variant.ron");
    let asset = create_prefab_variant(world, &base, &variant, Some(instance)).expect("from instance");
    let file = read_variant_file(&variant).expect("read");
    assert_eq!(file.base.overrides.len(), 1);
    let spawned =
        crate::content::spawn_prefab_asset(world, &variant, regia_core::math::Vec3::ZERO).expect("spawn");
    assert_eq!(
        world
            .get::<PrefabInstance>(spawned)
            .expect("record")
            .prefab_guid(),
        asset
    );
    let child = child_of_instance(world, spawned);
    assert!(world.get::<Tags>(child).expect("tags").contains("mossy"));

    // A variant of the variant (chain of two) spawns through both.
    let deeper = dir.join("assets/mossy_tall.variant.ron");
    create_prefab_variant(world, &variant, &deeper, None).expect("variant of variant");
    // A fresh world finds the whole chain in the project by id.
    let mut fresh = self::app(&dir);
    let tall = crate::content::spawn_prefab_asset(fresh.world_mut(), &deeper, regia_core::math::Vec3::ZERO)
        .expect("chain found by id");
    let child = child_of_instance(fresh.world_mut(), tall);
    assert!(fresh.world().get::<Tags>(child).expect("tags").contains("mossy"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn errors_carry_reasons() {
    let dir = project("variant-errors");
    let mut app = app(&dir);
    let world = app.world_mut();
    let base = write_lamp(&dir, 1.0);
    let reason = |r: Result<Guid, VariantEditError>| r.map_err(|e| e.reason);
    assert_eq!(
        reason(create_prefab_variant(
            world,
            &base,
            &dir.join("assets/x.prefab.ron"),
            None
        )),
        Err("bad_path")
    );
    assert_eq!(
        reason(create_prefab_variant(
            world,
            &dir.join("assets/none.prefab.ron"),
            &dir.join("assets/a.variant.ron"),
            None
        )),
        Err("base_missing")
    );
    assert_eq!(
        reason(create_prefab_variant(
            world,
            &dir.join("assets/x.png"),
            &dir.join("assets/a.variant.ron"),
            None
        )),
        Err("not_a_prefab")
    );
    let not_instance = world.spawn_empty().id();
    assert_eq!(
        reason(create_prefab_variant(
            world,
            &base,
            &dir.join("assets/b.variant.ron"),
            Some(not_instance)
        )),
        Err("not_an_instance")
    );
    // An instance of another prefab.
    let other = dir.join("assets/other.prefab.ron");
    std::fs::copy(&base, &other).expect("copy");
    let other_instance =
        crate::content::spawn_prefab_asset(world, &other, regia_core::math::Vec3::ZERO).expect("spawn");
    assert_eq!(
        reason(create_prefab_variant(
            world,
            &base,
            &dir.join("assets/c.variant.ron"),
            Some(other_instance)
        )),
        Err("instance_of_other_prefab")
    );
    // A variant whose base is nowhere.
    let orphan = dir.join("assets/orphan.variant.ron");
    let text = PrefabVariantFile::new(Guid::from_u128(0xdead))
        .to_ron_string()
        .expect("text");
    std::fs::write(&orphan, text).expect("write");
    assert_eq!(reason(register_prefab_file(world, &orphan)), Err("chain_broken"));
    // A broken file.
    let broken = dir.join("assets/broken.variant.ron");
    std::fs::write(&broken, "(format_version: 1, base: 7)").expect("write");
    assert_eq!(reason(register_prefab_file(world, &broken)), Err("bad_variant"));
    // An override value that is not RON.
    let ok = dir.join("assets/ok.variant.ron");
    create_prefab_variant(world, &base, &ok, None).expect("create");
    assert_eq!(
        set_variant_override(world, &ok, CHILD.parse().expect("guid"), TAGS, ".", Some("((("))
            .map_err(|e| e.reason),
        Err("bad_value")
    );
    assert_eq!(
        set_variant_override(world, &ok, Guid::NIL, TAGS, ".", Some("1")).map_err(|e| e.reason),
        Err("bad_value")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A base saved from prefab edit mode reaches the instances of its variants: they sit in the
/// stashed level during the save and come back from the saved template on close, with the
/// variant's override.
#[test]
fn a_base_saved_in_edit_mode_reaches_variant_instances_on_close() {
    let dir = project("variant-save-base");
    let mut app = app(&dir);
    let world = app.world_mut();
    let open = crate::level_io::new_level(world);
    world.insert_resource(open);
    world.insert_resource(crate::command::UndoStack::new());
    world.insert_resource(crate::prefab_edit::PrefabEditMode::default());
    let base = write_lamp(&dir, 1.0);
    let variant = dir.join("assets/props/red_lamp.variant.ron");
    let asset = create_prefab_variant(world, &base, &variant, None).expect("create");
    set_variant_override(
        world,
        &variant,
        CHILD.parse().expect("guid"),
        TAGS,
        ".",
        Some(r#"(["red"])"#),
    )
    .expect("set");
    crate::content::spawn_prefab_asset(world, &variant, regia_core::math::Vec3::ZERO).expect("spawn");

    crate::prefab_edit::enter_prefab_edit(world, &base).expect("enter");
    let child_guid: Guid = CHILD.parse().expect("guid");
    let mut by_guid = world.query::<(Entity, &regia_scene::prelude::EntityGuid)>();
    let edited = by_guid
        .iter(world)
        .find(|(_, guid)| guid.0 == child_guid)
        .map(|(entity, _)| entity)
        .expect("the base's child in edit mode");
    world.get_mut::<Transform>(edited).expect("t").translation.y = 5.0;
    let updated = crate::prefab_edit::save_prefab(world).expect("save");
    assert_eq!(updated, 0, "no live instances in the isolated prefab scene");
    crate::prefab_edit::exit_prefab_edit(world, false).expect("exit");

    let instance = instances_of(world, asset)[0];
    let child = child_of_instance(world, instance);
    assert_eq!(world.get::<Transform>(child).expect("t").translation.y, 5.0);
    assert!(world.get::<Tags>(child).expect("tags kept").contains("red"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Variants have no edit mode (spec 83 § 9): a variant is refused by name, not as a parse error.
#[test]
fn prefab_edit_mode_refuses_a_variant_by_name() {
    let dir = project("variant-no-edit-mode");
    let mut app = app(&dir);
    let world = app.world_mut();
    let base = write_lamp(&dir, 1.0);
    let variant = dir.join("assets/props/red_lamp.variant.ron");
    create_prefab_variant(world, &base, &variant, None).expect("create");
    let err = crate::prefab_edit::enter_prefab_edit(world, &variant).expect_err("refused");
    assert!(err.contains("prefab variant"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A loop is refused at registration with the documented reason, so `editor.prefab_variant` and
/// `editor.spawn_prefab` name it instead of showing a cut chain.
#[test]
fn a_variant_loop_is_chain_broken() {
    let dir = project("variant-loop");
    let mut app = app(&dir);
    let world = app.world_mut();
    let first = dir.join("assets/first.variant.ron");
    let second = dir.join("assets/second.variant.ron");
    let (first_id, second_id) = (Guid::new_random(), Guid::new_random());
    for (path, id, base) in [(&first, first_id, second_id), (&second, second_id, first_id)] {
        let meta = regia_asset::MetaSidecar::new(VARIANT_META_KIND, regia_asset::AssetId(id));
        let meta = meta.to_ron_string().expect("meta");
        std::fs::write(regia_asset::meta_path_for(path), meta).expect("meta");
        let text = PrefabVariantFile::new(base).to_ron_string().expect("text");
        std::fs::write(path, text).expect("variant");
    }
    assert_eq!(
        register_prefab_file(world, &first).map_err(|e| e.reason),
        Err("chain_broken")
    );
    assert_eq!(
        describe_variant(world, &second).map(|_| ()).map_err(|e| e.reason),
        Err("chain_broken")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_methods_create_read_and_edit() {
    let dir = project("variant-agent");
    let mut app = app(&dir);
    let world = app.world_mut();
    write_lamp(&dir, 1.0);
    let handlers: std::collections::HashMap<&str, Handler> = methods()
        .into_iter()
        .map(|(schema, handler)| (schema.name, handler))
        .collect();
    let call = |world: &mut World, name: &str, params: Value| handlers[name](world, &params);
    let reason = |err: RpcError| err.data.and_then(|d| d["reason"].as_str().map(str::to_string));

    let created = call(
        world,
        "editor.create_prefab_variant",
        json!({ "base": "assets/props/deep/lamp.prefab.ron", "path": "assets/v/red.variant.ron" }),
    )
    .expect("create");
    assert_eq!(created["overrides"], 0);
    let set = call(
        world,
        "editor.set_variant_override",
        json!({ "path": "v/red.variant.ron", "target": CHILD, "component": TAGS, "value": "([\"red\"])" }),
    )
    .expect("set");
    assert_eq!(set["overrides"], 1);
    let read = call(
        world,
        "editor.prefab_variant",
        json!({ "path": "assets/v/red.variant.ron" }),
    )
    .expect("read");
    assert_eq!(read["chain"].as_array().map(Vec::len), Some(2));
    assert_eq!(read["overrides"][0]["component"], TAGS);
    assert_eq!(read["asset"], created["asset"]);

    let err = call(
        world,
        "editor.prefab_variant",
        json!({ "path": "../x.variant.ron" }),
    )
    .expect_err("escape");
    assert_eq!(reason(err).as_deref(), Some("bad_path"));
    let err = call(
        world,
        "editor.prefab_variant",
        json!({ "path": "assets/missing.variant.ron" }),
    )
    .expect_err("missing");
    assert_eq!(reason(err).as_deref(), Some("variant_missing"));
    let err = call(
        world,
        "editor.set_variant_override",
        json!({ "path": "assets/missing.variant.ron", "target": CHILD, "component": TAGS, "value": "([])" }),
    )
    .expect_err("missing override target");
    assert_eq!(reason(err).as_deref(), Some("variant_missing"));
    let err = call(
        world,
        "editor.prefab_variant",
        json!({ "path": "assets/props/deep/lamp.prefab.ron" }),
    )
    .expect_err("not a variant");
    assert_eq!(reason(err).as_deref(), Some("bad_path"));
    let err = call(
        world,
        "editor.create_prefab_variant",
        json!({ "base": "assets/props/deep/lamp.prefab.ron", "path": "assets/v/x.variant.ron", "from_entity": 12345678901u64 }),
    )
    .expect_err("no entity");
    assert_eq!(reason(err).as_deref(), Some("entity_missing"));
    world.remove_resource::<crate::agent_ext::EditorProject>();
    let err = call(
        world,
        "editor.prefab_variant",
        json!({ "path": "assets/v/red.variant.ron" }),
    )
    .expect_err("no project");
    assert_eq!(reason(err).as_deref(), Some("no_project"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec 83 § 2.4: `removed_children` is a count, like the other three edit fields.
#[test]
fn prefab_variant_removed_children_is_a_count() {
    let dir = project("variant-removed-count");
    let mut app = app(&dir);
    let world = app.world_mut();
    let base = write_lamp(&dir, 1.0);
    let variant = dir.join("assets/props/red_lamp.variant.ron");
    create_prefab_variant(world, &base, &variant, None).expect("create");
    let empty = describe_variant(world, &variant).expect("describe");
    assert_eq!(empty["added_children"], json!(0));
    assert_eq!(empty["removed_children"], json!(0));
    assert!(empty["removed_children"].is_number());

    let mut file = read_variant_file(&variant).expect("read");
    file.base.removed_children.push(CHILD.parse().expect("guid"));
    let text = file.to_ron_string().expect("ron");
    std::fs::write(&variant, text).expect("write");
    let one = describe_variant(world, &variant).expect("describe again");
    assert_eq!(one["removed_children"], json!(1));
    assert!(one["removed_children"].is_number());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The reasons the code returns: the first argument of every `VariantEditError::new(...)`.
fn reasons_in_code(source: &'static str) -> Vec<&'static str> {
    let mut reasons = Vec::new();
    for piece in source.split("VariantEditError::new(").skip(1) {
        let piece = piece.trim_start();
        if let Some(rest) = piece.strip_prefix('"')
            && let Some(end) = rest.find('"')
        {
            reasons.push(&rest[..end]);
        }
    }
    reasons.sort_unstable();
    reasons.dedup();
    reasons
}

/// Spec 83 § 2.4 and AGENTS.md rule 8: every reason the variant methods return is listed in the
/// module docs and in `book/agents/prefab-variants.md`, with the `F` code `rpc` maps it to.
#[test]
fn the_book_page_names_every_reason_with_its_code() {
    let module = include_str!("../prefab_variant_edit.rs");
    let book = include_str!("../../../../book/agents/prefab-variants.md");
    let reasons = reasons_in_code(module);
    // Guard the scan itself: these reasons are built across line breaks or in helpers.
    for known in [
        "no_project",
        "chain_broken",
        "io_failed",
        "instance_of_other_prefab",
    ] {
        assert!(reasons.contains(&known), "{known} found in the code: {reasons:?}");
    }

    let docs: String = module
        .lines()
        .skip_while(|line| !line.starts_with("//! Error reasons"))
        .take_while(|line| line.starts_with("//!"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut listed: Vec<&str> = docs
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|r| *r != "data.reason")
        .collect();
    listed.sort_unstable();
    assert_eq!(
        listed, reasons,
        "the module docs list exactly the reasons the code returns"
    );

    for reason in reasons {
        let err = rpc(VariantEditError::new(reason, "test"));
        let data = err.data.expect("an rpc error carries data");
        assert_eq!(data["reason"], json!(reason));
        let code = data["code"].as_str().expect("an F code");
        let row = format!("| `{reason}` | {code} |");
        assert!(
            book.contains(&row),
            "book/agents/prefab-variants.md has the row `{row}`"
        );
    }
}

/// The files in `dir` (names only, sorted).
fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// rc1 editor-undo finding 9 (ADR-7412): variant and `.meta` saves used a hand-rolled
/// `<name>.new.tmp` + rename that left the temp file behind when the rename failed. They go
/// through the editor's shared atomic write now, which removes its temp file on failure.
#[test]
fn a_failed_variant_write_leaves_no_temp_file() {
    let dir = project("variant-write-fail");
    let folder = dir.join("assets/props");
    // The target is a non-empty directory: the rename over it fails on every platform.
    let target = folder.join("lamp_red.variant.ron");
    std::fs::create_dir_all(target.join("inside")).expect("blocking dir");
    let before = listing(&folder);

    let err = write_atomic(&target, b"( base: \"x\" )").expect_err("a directory cannot be replaced");
    assert_eq!(err.reason, "io_failed", "{err:?}");
    assert_eq!(
        listing(&folder),
        before,
        "no temp file is left next to the variant"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// rc2 review (rev-zai-rc-b editor-undo finding 4): `respawn_with_dependents` rebuilt the base's
/// instances and then each variant's in separate passes, so a level-authored instance whose
/// template holds both the base and a variant of it was respawned twice and counted twice in
/// `instances_updated`. One pass rebuilds it once and counts it once.
#[test]
fn a_root_holding_a_base_and_its_variant_updates_once() {
    let dir = project("variant-shelf-once");
    let mut app = app(&dir);
    let world = app.world_mut();
    let open = crate::level_io::new_level(world);
    world.insert_resource(open);
    world.insert_resource(crate::command::UndoStack::new());
    let base = write_lamp(&dir, 1.0);
    let variant = dir.join("assets/props/red_lamp.variant.ron");
    let red = create_prefab_variant(world, &base, &variant, None).expect("create");
    let lamp = crate::content::prefab_asset_id(&base).expect("lamp id").0;
    // A shelf prefab holding one lamp and one red lamp.
    let shelf = dir.join("assets/props/shelf.prefab.ron");
    let instance = |asset: Guid| {
        format!(
            "prefab_instance: Some(( prefab: \"asset:{asset}\", overrides: [], added_components: [], \
             removed_components: [], added_children: [], removed_children: [] ))"
        )
    };
    let text = format!(
        r#"( format_version: 1, entities: [
            ( guid: "8b6f2a11-0000-4000-8000-0000000000c1", name: "Shelf",
              components: {{ "regia_scene::transform::Transform": () }} ),
            ( guid: "8b6f2a11-0000-4000-8000-0000000000c2", name: "Lamp", parent: Some("8b6f2a11-0000-4000-8000-0000000000c1"),
              components: {{ "regia_scene::transform::Transform": () }}, {} ),
            ( guid: "8b6f2a11-0000-4000-8000-0000000000c3", name: "RedLamp", parent: Some("8b6f2a11-0000-4000-8000-0000000000c1"),
              components: {{ "regia_scene::transform::Transform": () }}, {} ),
        ] )"#,
        instance(lamp),
        instance(red)
    );
    regia_test_utils::write_atomic(&shelf, text.as_bytes()).expect("shelf");
    crate::content::spawn_prefab_asset(world, &shelf, regia_core::math::Vec3::ZERO).expect("spawn the shelf");
    assert_eq!(instances_of(world, lamp).len(), 1, "the shelf holds one lamp");
    assert_eq!(instances_of(world, red).len(), 1, "and one red lamp");

    write_lamp(&dir, 3.0);
    let updated = crate::prefab_edit::reload_prefab_file(world, &base).expect("reload");
    assert_eq!(
        updated, 1,
        "one level-authored root (the shelf) was rebuilt, once"
    );
    for asset in [lamp, red] {
        let found = instances_of(world, asset);
        assert_eq!(found.len(), 1, "still one instance of {asset}");
        let child = child_of_instance(world, found[0]);
        assert_eq!(world.get::<Transform>(child).expect("t").translation.y, 3.0);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// rc2 review (rev-zai-rc-b editor-undo finding 1), variant half: a base edited on disk into a
/// template that nests itself through its own variant fails its reload; the variant's instance
/// survives it with the old base, and the library keeps the old base template.
#[test]
fn a_base_that_nests_itself_keeps_the_variant_instances() {
    let dir = project("variant-base-cycle");
    let mut app = app(&dir);
    let world = app.world_mut();
    let open = crate::level_io::new_level(world);
    world.insert_resource(open);
    world.insert_resource(crate::command::UndoStack::new());
    let base = write_lamp(&dir, 1.0);
    let variant = dir.join("assets/props/red_lamp.variant.ron");
    let red = create_prefab_variant(world, &base, &variant, None).expect("create");
    let lamp = crate::content::prefab_asset_id(&base).expect("lamp id").0;
    crate::content::spawn_prefab_asset(world, &variant, regia_core::math::Vec3::ZERO).expect("spawn");
    assert_eq!(instances_of(world, red).len(), 1);

    // The base now holds an instance of its own variant: base -> variant -> base -> ...
    let text = format!(
        r#"( format_version: 1, entities: [
            ( guid: "{ROOT}", name: "Root", components: {{ "regia_scene::transform::Transform": () }} ),
            ( guid: "{CHILD}", name: "Child", parent: Some("{ROOT}"),
              components: {{ "regia_scene::transform::Transform": ( translation: (0.0, 9.0, 0.0) ) }} ),
            ( guid: "8b6f2a11-0000-4000-8000-0000000000d1", name: "Loop", parent: Some("{ROOT}"),
              prefab_instance: Some(( prefab: "asset:{red}", overrides: [], added_components: [],
                removed_components: [], added_children: [], removed_children: [] )) ),
        ] )"#
    );
    regia_test_utils::write_atomic(&base, text.as_bytes()).expect("rewrite the base");
    let err = crate::prefab_edit::reload_prefab_file(world, &base).expect_err("the base nests itself");
    assert!(err.contains("previous"), "{err}");
    let found = instances_of(world, red);
    assert_eq!(found.len(), 1, "the variant's instance is still in the level");
    let child = child_of_instance(world, found[0]);
    assert_eq!(
        world.get::<Transform>(child).expect("t").translation.y,
        1.0,
        "it keeps the old base"
    );
    assert!(
        world
            .resource::<PrefabLibrary>()
            .get(lamp)
            .is_some_and(|template| template.entities.len() == 2),
        "the library keeps the old base template"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The lamp with a fixed `.meta` id, so two projects write the same variant bytes.
fn write_lamp_with_id(dir: &Path) -> PathBuf {
    let base = write_lamp(dir, 1.0);
    let id = Guid::from_u128(0x1a3b_0000_0000_4000_8000_0000_0000_00aa);
    let meta = regia_asset::MetaSidecar::new("prefab_ron", regia_asset::AssetId(id));
    regia_test_utils::write_atomic(
        &regia_asset::meta_path_for(&base),
        meta.to_ron_string().expect("meta").as_bytes(),
    )
    .expect("meta");
    base
}

/// A project with the lamp and one instance of it carrying a Tags override on the child.
fn instance_project(tag: &str) -> (PathBuf, App, Entity) {
    let dir = project(tag);
    let mut app = app(&dir);
    app.world_mut().insert_resource(crate::command::UndoStack::new());
    let base = write_lamp_with_id(&dir);
    let world = app.world_mut();
    let instance =
        crate::content::spawn_prefab_asset(world, &base, regia_core::math::Vec3::ZERO).expect("spawn");
    world
        .get_mut::<PrefabInstance>(instance)
        .expect("record")
        .overrides
        .push(regia_scene::prefab::OverrideEntry {
            target: CHILD.parse::<Guid>().expect("guid").as_u128(),
            component: TAGS.into(),
            path: ".".into(),
            value: r#"(["mossy"])"#.into(),
        });
    (dir, app, instance)
}

/// ADR-7614: the Outliner's Create variant… (its dialog) writes the file
/// `editor.create_prefab_variant` writes, and is one undo step whose undo removes the file.
#[test]
fn create_variant_from_the_ui_writes_the_agent_file_and_is_one_undo_step() {
    let (agent_dir, mut agent_app, agent_instance) = instance_project("variant-ui-agent");
    let (ui_dir, mut ui_app, ui_instance) = instance_project("variant-ui-dialog");
    let handler = methods()
        .into_iter()
        .find(|(schema, _)| schema.name == "editor.create_prefab_variant")
        .map(|(_, handler)| handler)
        .expect("registered");
    handler(
        agent_app.world_mut(),
        &json!({
            "base": "assets/props/deep/lamp.prefab.ron",
            "path": "assets/v/mossy.variant.ron",
            "from_entity": agent_instance.to_bits(),
        }),
    )
    .expect("agent create");

    let world = ui_app.world_mut();
    let base = instance_base_path(world, ui_instance).expect("the instance's base file");
    assert_eq!(base, "props/deep/lamp.prefab.ron");
    let dialog = CreateVariantDialog {
        base: base.clone(),
        path: "v/mossy.variant.ron".into(),
        from: Some(ui_instance),
        ..CreateVariantDialog::default()
    };
    assert_eq!(default_variant_path(&base), "props/deep/lamp_variant.variant.ron");
    let depth = world.resource::<crate::command::UndoStack>().undo_depth();
    create_variant_from_dialog(world, &dialog).expect("dialog create");
    let agent_bytes = std::fs::read(agent_dir.join("assets/v/mossy.variant.ron")).expect("agent file");
    let ui_path = ui_dir.join("assets/v/mossy.variant.ron");
    let ui_bytes = std::fs::read(&ui_path).expect("ui file");
    assert_eq!(
        String::from_utf8_lossy(&ui_bytes),
        String::from_utf8_lossy(&agent_bytes),
        "the same file"
    );
    assert_eq!(
        read_variant_file(&ui_path).expect("read").base.overrides.len(),
        1,
        "the instance's override is kept"
    );
    assert_eq!(
        world.resource::<crate::command::UndoStack>().undo_depth(),
        depth + 1
    );
    let meta = std::fs::read(regia_asset::meta_path_for(&ui_path)).expect("meta");

    assert!(crate::command::undo_world(world));
    assert!(!ui_path.exists(), "undo removes the variant");
    assert!(!regia_asset::meta_path_for(&ui_path).exists(), "and its meta");
    assert!(crate::command::redo_world(world));
    assert_eq!(std::fs::read(&ui_path).expect("redo"), ui_bytes);
    assert_eq!(
        std::fs::read(regia_asset::meta_path_for(&ui_path)).expect("meta"),
        meta,
        "the same id"
    );
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::remove_dir_all(&ui_dir);
}

/// ADR-7744: the Create variant dialog is drawn by the shell, so the palette's
/// `prefab.create_variant` opens it with the Content Browser and the Outliner both closed: only
/// `variant_dialog` runs (as the shell's frame does), and the dialog state reports open.
#[test]
fn the_palette_opens_the_variant_dialog_with_no_panel_drawn() {
    let (dir, mut app, instance) = instance_project("variant-palette");
    let world = app.world_mut();
    let ctx = egui::Context::default();
    // A selection that is no prefab instance opens nothing.
    world.insert_resource(crate::selection::Selection::default());
    let line = crate::command_palette::run(world, "prefab.create_variant").expect("a command");
    assert!(line.starts_with("Select one prefab instance"), "{line}");
    ctx.run_ui(egui::RawInput::default(), |ui| variant_dialog(ui.ctx(), world))
        .textures_delta
        .clear();
    assert!(!dialog_open(&ctx), "nothing was asked for");

    // One selected instance: the command asks, the shell's draw opens the dialog.
    let mut selection = crate::selection::Selection::default();
    selection.select(instance, crate::selection::SelectMode::Replace);
    world.insert_resource(selection);
    let line = crate::command_palette::run(world, "prefab.create_variant").expect("a command");
    assert!(line.contains("props/deep/lamp.prefab.ron"), "{line}");
    assert!(!dialog_open(&ctx), "asking is not drawing");
    ctx.run_ui(egui::RawInput::default(), |ui| variant_dialog(ui.ctx(), world))
        .textures_delta
        .clear();
    assert!(dialog_open(&ctx));
    assert_eq!(
        dialog_target(&ctx),
        Some((
            "props/deep/lamp.prefab.ron".to_owned(),
            "props/deep/lamp_variant.variant.ron".to_owned()
        ))
    );
    assert!(
        world.resource::<VariantDialogRequest>().pending.is_none(),
        "the request was taken"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// rev-claude-64 (3) finding 4: creating a variant writes a file only; its undo step does not
/// mark the saved level dirty (no close prompt, *Edit placed level* not refused `level_dirty`),
/// and undoing it leaves the level clean too.
#[test]
fn creating_a_variant_leaves_a_saved_level_clean() {
    let (dir, mut app, instance) = instance_project("variant-clean");
    let world = app.world_mut();
    world.resource_mut::<crate::command::UndoStack>().mark_saved();
    let depth = world.resource::<crate::command::UndoStack>().undo_depth();
    let base = dir.join("assets/props/deep/lamp.prefab.ron");
    let variant = dir.join("assets/v/clean.variant.ron");
    create_prefab_variant(world, &base, &variant, Some(instance)).expect("create");
    let stack = world.resource::<crate::command::UndoStack>();
    assert_eq!(stack.undo_depth(), depth + 1, "still one undo step");
    assert!(!stack.is_dirty(), "a file-only step leaves the level clean");
    assert!(crate::command::undo_world(world));
    assert!(!variant.exists(), "undo removes the variant");
    assert!(!world.resource::<crate::command::UndoStack>().is_dirty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A project with a variant of the lamp overriding the child's Tags, and an instance of it.
fn variant_project(tag: &str) -> (PathBuf, App, Entity) {
    let dir = project(tag);
    let mut app = app(&dir);
    let base = write_lamp_with_id(&dir);
    let variant = dir.join("assets/v/red.variant.ron");
    let world = app.world_mut();
    let asset = create_prefab_variant(world, &base, &variant, None).expect("create");
    set_variant_override(
        world,
        &variant,
        CHILD.parse().expect("guid"),
        TAGS,
        ".",
        Some(r#"(["red"])"#),
    )
    .expect("override");
    crate::content::spawn_prefab_asset(world, &variant, regia_core::math::Vec3::ZERO).expect("spawn");
    let instance = instances_of(world, asset)[0];
    world.insert_resource(crate::command::UndoStack::new());
    (dir, app, instance)
}

/// ADR-7614: the Inspector lists the variant's override with its marker row, and Clear override
/// writes the file `editor.set_variant_override { value: null }` writes, as one undo step.
#[test]
fn clear_override_from_the_inspector_writes_the_agent_file_and_is_one_undo_step() {
    let (agent_dir, mut agent_app, _) = variant_project("variant-clear-agent");
    let (ui_dir, mut ui_app, ui_instance) = variant_project("variant-clear-ui");
    let before = std::fs::read(ui_dir.join("assets/v/red.variant.ron")).expect("before");
    let handler = methods()
        .into_iter()
        .find(|(schema, _)| schema.name == "editor.set_variant_override")
        .map(|(_, handler)| handler)
        .expect("registered");
    handler(
        agent_app.world_mut(),
        &json!({ "path": "assets/v/red.variant.ron", "target": CHILD, "component": TAGS, "value": null }),
    )
    .expect("agent clear");

    let world = ui_app.world_mut();
    let child = child_of_instance(world, ui_instance);
    assert!(
        crate::inspector_variant::variant_overrides(world, child, "regia_scene::transform::Transform")
            .is_empty()
    );
    let rows = crate::inspector_variant::variant_overrides(world, child, TAGS);
    assert_eq!(rows.len(), 1, "the overridden field is marked");
    assert_eq!(rows[0].path, ".");
    assert_eq!(rows[0].target, CHILD.parse::<Guid>().expect("guid"));
    let depth = world.resource::<crate::command::UndoStack>().undo_depth();
    let updated = crate::inspector_variant::clear_variant_override(world, &rows[0]).expect("clear");
    assert_eq!(updated, 1, "the live instance updated");
    let ui_path = ui_dir.join("assets/v/red.variant.ron");
    let ui_bytes = std::fs::read(&ui_path).expect("ui file");
    let agent_bytes = std::fs::read(agent_dir.join("assets/v/red.variant.ron")).expect("agent file");
    assert_eq!(
        String::from_utf8_lossy(&ui_bytes),
        String::from_utf8_lossy(&agent_bytes),
        "the same file"
    );
    assert_eq!(
        world.resource::<crate::command::UndoStack>().undo_depth(),
        depth + 1
    );
    let asset = rows[0].variant;
    let instance = instances_of(world, asset)[0];
    let child = child_of_instance(world, instance);
    assert!(crate::inspector_variant::variant_overrides(world, child, TAGS).is_empty());
    assert!(crate::command::undo_world(world));
    assert_eq!(
        std::fs::read(&ui_path).expect("undo"),
        before,
        "undo restores the override"
    );
    let _ = std::fs::remove_dir_all(&agent_dir);
    let _ = std::fs::remove_dir_all(&ui_dir);
}
