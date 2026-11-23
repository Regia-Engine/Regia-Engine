// SPDX-License-Identifier: MPL-2.0
//! The Overrides window, its Apply targets and Edit in place (UI parity addendum § 9.1-9.2,
//! ADR-7790).

use std::path::{Path, PathBuf};

use regia_app::prelude::App;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_scene::prefab::{PrefabInstance, PrefabLink};
use regia_scene::prelude::{EntityGuid, SceneFile, Transform};
use serde_json::json;

use super::*;
use crate::command::{UndoStack, redo_world, undo_world};
use crate::inspector;

/// The crate prefab's lid (one meter up).
const LID: u128 = 0x8b6f_2a11_0000_4000_8000_0000_0000_0002;
/// The crate prefab's root.
const CRATE_ROOT: u128 = 0x8b6f_2a11_0000_4000_8000_0000_0000_0001;
/// The stack prefab's root (it nests the crate under its second record).
const STACK_ROOT: u128 = 0x6e0c_0000_0000_4000_8000_0000_0000_0001;

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

/// The crate plus a "stack" prefab that nests it; both registered. Returns (crate, stack).
fn write_stack(app: &mut App, dir: &Path) -> (PathBuf, PathBuf) {
    let inner = write_crate(dir);
    let inner_id = crate::content::prefab_asset_id(&inner).expect("meta").0;
    let outer = dir.join("assets/props/stack.prefab.ron");
    let text = format!(
        r#"( format_version: 1, entities: [
        ( guid: "6e0c0000-0000-4000-8000-000000000001", name: "Stack",
          components: {{ "regia_scene::transform::Transform": () }} ),
        ( guid: "6e0c0000-0000-4000-8000-000000000002", parent: Some("6e0c0000-0000-4000-8000-000000000001"),
          components: {{ "regia_scene::transform::Transform": () }},
          prefab_instance: ( prefab: "asset:{inner_id}" ) ),
    ] )"#
    );
    std::fs::write(&outer, text).expect("outer");
    crate::prefab_variant_edit::register_prefab_file(app.world_mut(), &inner).expect("inner");
    (inner, outer)
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

fn translation(world: &mut World, root: Entity, local: u128) -> Vec3 {
    let entity = member(world, root, local);
    world.get::<Transform>(entity).expect("transform").translation
}

fn set_translation(world: &mut World, root: Entity, local: u128, at: Vec3) {
    let entity = member(world, root, local);
    inspector::edit_value(world, &[entity], "Transform", "translation", Box::new(at)).expect("edit");
}

/// The text of `component` on the record `guid` of a prefab file.
fn record_component(text: &str, guid: u128) -> String {
    let file = SceneFile::from_ron_str(text).expect("parses");
    let record = file
        .entities
        .iter()
        .find(|record| record.guid == Guid::from_u128(guid))
        .expect("record");
    record
        .components
        .values()
        .map(|raw| raw.get_ron().to_string())
        .collect::<Vec<_>>()
        .join(";")
}

fn reason(err: &regia_agent::protocol::RpcError) -> Option<String> {
    err.data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(|reason| reason.as_str())
        .map(str::to_string)
}

/// Acceptance (P-163): on a nested fixture, one field applied to the outer prefab and another to
/// the inner (nested) prefab write the expected bytes into each file, and one undo restores both.
#[test]
fn nested_fixture_applies_to_outer_and_inner_in_one_undo_step() {
    let (mut app, dir) = app_with_project("overrides-nested-acceptance");
    let (inner, outer) = write_stack(&mut app, &dir);
    let inner_before = std::fs::read(&inner).expect("inner");
    let outer_before = std::fs::read(&outer).expect("outer");
    let one = crate::content::spawn_prefab_asset(app.world_mut(), &outer, Vec3::ZERO).expect("one");
    let two = crate::content::spawn_prefab_asset(app.world_mut(), &outer, Vec3::X).expect("two");
    set_translation(app.world_mut(), one, LID, Vec3::new(0.0, 3.0, 0.0));
    set_translation(app.world_mut(), one, STACK_ROOT, Vec3::new(0.0, 0.0, 2.0));
    let lid = member(app.world_mut(), one, LID);
    let stack_root = member(app.world_mut(), one, STACK_ROOT);
    let depth = app.world().resource::<UndoStack>().undo_depth();

    let report = apply_rows(
        app.world_mut(),
        &[
            RowRef {
                entity: stack_root,
                component: "Transform".into(),
                path: "translation".into(),
                target: Some(ApplyTarget::Outer),
            },
            RowRef {
                entity: lid,
                component: "Transform".into(),
                path: "translation".into(),
                target: Some(ApplyTarget::Nested),
            },
        ],
    )
    .expect("apply both");
    assert_eq!(report.done, 2);
    assert_eq!(report.files, vec![outer.clone(), inner.clone()]);
    assert_eq!(
        app.world().resource::<UndoStack>().undo_depth(),
        depth + 1,
        "both applies are one undo step"
    );

    // The inner file: the lid's own record holds 3.0, the crate root is as it was, and the bytes
    // are canonical (they parse back to the same text).
    let inner_text = std::fs::read_to_string(&inner).expect("inner");
    let inner_file = SceneFile::from_ron_str(&inner_text).expect("parse");
    assert_eq!(
        inner_file.to_ron_string().expect("ron"),
        inner_text,
        "canonical bytes"
    );
    assert!(record_component(&inner_text, LID).contains("3.0"), "{inner_text}");
    let before_text = String::from_utf8(inner_before.clone()).expect("utf8");
    assert_eq!(
        record_component(&inner_text, CRATE_ROOT),
        record_component(&before_text, CRATE_ROOT),
        "the crate root record is unchanged"
    );
    // The outer file: the stack root's record holds 2.0; the nested record carries no override
    // (the lid went to the inner prefab, not as an outer-wins override).
    let outer_text = std::fs::read_to_string(&outer).expect("outer");
    let outer_file = SceneFile::from_ron_str(&outer_text).expect("parse");
    assert_eq!(
        outer_file.to_ron_string().expect("ron"),
        outer_text,
        "canonical bytes"
    );
    assert!(
        record_component(&outer_text, STACK_ROOT).contains("2.0"),
        "{outer_text}"
    );
    let nested = outer_file
        .entities
        .iter()
        .find_map(|record| record.prefab_instance.as_ref())
        .expect("nested record");
    assert!(nested.overrides.is_empty(), "{:?}", nested.overrides);

    // Both overrides left instance one; instance two follows both files.
    let world = app.world_mut();
    let rows = override_rows(world, one).expect("rows");
    assert!(
        rows.rows.iter().all(|row| row.kind != RowKind::Field),
        "{:?}",
        rows.rows
    );
    assert_eq!(translation(world, two, LID), Vec3::new(0.0, 3.0, 0.0));
    assert_eq!(translation(world, two, STACK_ROOT), Vec3::new(0.0, 0.0, 2.0));
    let inner_after = std::fs::read(&inner).expect("inner");
    let outer_after = std::fs::read(&outer).expect("outer");

    // One undo: both files' bytes and both overrides come back; instance two goes back.
    assert!(undo_world(app.world_mut()));
    assert_eq!(std::fs::read(&inner).expect("inner"), inner_before);
    assert_eq!(std::fs::read(&outer).expect("outer"), outer_before);
    let world = app.world_mut();
    let fields = override_rows(world, one)
        .expect("rows")
        .rows
        .into_iter()
        .filter(|row| row.kind == RowKind::Field)
        .count();
    assert_eq!(fields, 2, "both overrides are back");
    assert_eq!(translation(world, one, LID), Vec3::new(0.0, 3.0, 0.0));
    assert_eq!(translation(world, two, LID), Vec3::new(0.0, 1.0, 0.0));
    assert_eq!(translation(world, two, STACK_ROOT), Vec3::ZERO);
    // Redo writes the same bytes again.
    assert!(redo_world(app.world_mut()));
    assert_eq!(std::fs::read(&inner).expect("inner"), inner_after);
    assert_eq!(std::fs::read(&outer).expect("outer"), outer_after);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The rows of a nested instance: prefab and instance values side by side, the valid targets of
/// each field and the default the inspector's Apply picks, and a child the level added.
#[test]
fn rows_show_both_values_targets_and_added_children() {
    let (mut app, dir) = app_with_project("overrides-rows");
    let (inner, outer) = write_stack(&mut app, &dir);
    let one = crate::content::spawn_prefab_asset(app.world_mut(), &outer, Vec3::ZERO).expect("one");
    set_translation(app.world_mut(), one, LID, Vec3::new(0.0, 3.0, 0.0));
    set_translation(app.world_mut(), one, STACK_ROOT, Vec3::new(0.0, 0.0, 2.0));
    let lid = member(app.world_mut(), one, LID);
    let added = app
        .world_mut()
        .spawn((Transform::default(), EntityGuid::new_random(), ChildOf(lid)))
        .id();

    let list = override_rows(app.world(), lid).expect("rows");
    assert_eq!(list.root, one, "any member resolves to the level-authored root");
    let lid_row = list
        .rows
        .iter()
        .find(|row| row.kind == RowKind::Field && row.entity == lid.to_bits())
        .expect("lid row");
    assert_eq!(lid_row.path, "translation");
    assert!(lid_row.prefab_value.contains("1.0"), "{lid_row:?}");
    assert!(lid_row.instance_value.contains("3.0"), "{lid_row:?}");
    assert_eq!(
        lid_row.targets,
        vec![
            ApplyChoice {
                target: ApplyTarget::Nested,
                file: inner.clone()
            },
            ApplyChoice {
                target: ApplyTarget::Outer,
                file: outer.clone()
            },
        ]
    );
    assert_eq!(
        lid_row.default_target,
        Some(ApplyTarget::Outer),
        "the inspector's Apply writes the outer file"
    );
    let root_row = list
        .rows
        .iter()
        .find(|row| row.kind == RowKind::Field && row.entity != lid.to_bits())
        .expect("stack root row");
    assert_eq!(
        root_row.targets.iter().map(|c| c.target).collect::<Vec<_>>(),
        vec![ApplyTarget::Outer],
        "an entity of the outer prefab has no nested target"
    );
    assert!(
        list.rows
            .iter()
            .any(|row| row.kind == RowKind::AddedChild && row.entity == added.to_bits() && !row.editable()),
        "{:?}",
        list.rows
    );
    // A plain entity is no instance.
    let plain = app.world_mut().spawn(Transform::default()).id();
    assert_eq!(
        override_rows(app.world(), plain).expect_err("plain").code,
        "not_prefab_instance"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A variant instance's override: the variant file (default) or the base; Apply to the base
/// writes the base file, leaves the variant file, and every instance of the base and the variant
/// follows. A variant step that overrides the field takes the base out of the choices.
#[test]
fn a_variant_override_applies_to_the_base_when_asked() {
    let (mut app, dir) = app_with_project("overrides-variant-base");
    let base = write_crate(&dir);
    let variant = dir.join("assets/props/crate_tall.variant.ron");
    crate::prefab_variant_edit::create_prefab_variant(app.world_mut(), &base, &variant, None)
        .expect("variant");
    let variant_before = std::fs::read(&variant).expect("variant");
    let base_before = std::fs::read(&base).expect("base");
    let a = crate::content::spawn_prefab_asset(app.world_mut(), &variant, Vec3::ZERO).expect("a");
    let b = crate::content::spawn_prefab_asset(app.world_mut(), &variant, Vec3::X).expect("b");
    let plain = crate::content::spawn_prefab_asset(app.world_mut(), &base, Vec3::Z).expect("plain");
    set_translation(app.world_mut(), a, LID, Vec3::new(0.0, 6.0, 0.0));
    let lid_a = member(app.world_mut(), a, LID);

    let (choices, default) =
        crate::inspector::multi::prefab_apply_targets(app.world(), lid_a, "Transform", "translation");
    assert_eq!(
        choices.iter().map(|c| c.target).collect::<Vec<_>>(),
        vec![ApplyTarget::Variant, ApplyTarget::Base]
    );
    assert_eq!(default, Some(ApplyTarget::Variant));

    crate::inspector::multi::apply_prefab_override_to(
        app.world_mut(),
        lid_a,
        "Transform",
        "translation",
        Some(ApplyTarget::Base),
    )
    .expect("apply to base");
    assert_eq!(
        std::fs::read(&variant).expect("variant"),
        variant_before,
        "the variant file is untouched"
    );
    let base_text = std::fs::read_to_string(&base).expect("base");
    assert!(record_component(&base_text, LID).contains("6.0"), "{base_text}");
    let world = app.world_mut();
    assert!(world.get::<PrefabInstance>(a).expect("a").overrides.is_empty());
    assert_eq!(
        translation(world, b, LID).y,
        6.0,
        "a variant instance follows its base"
    );
    assert_eq!(translation(world, plain, LID).y, 6.0, "a base instance follows");
    assert!(undo_world(app.world_mut()));
    assert_eq!(std::fs::read(&base).expect("base"), base_before);
    assert_eq!(translation(app.world_mut(), b, LID).y, 1.0);

    // A variant step that sets the field shadows the base: not offered, refused when asked.
    crate::prefab_variant_edit::set_variant_override(
        app.world_mut(),
        &variant,
        Guid::from_u128(LID),
        "regia_scene::transform::Transform",
        "translation",
        Some("(0.0, 2.0, 0.0)"),
    )
    .expect("variant override");
    let a = crate::command::resolve_entity(app.world(), a.to_bits()).expect("a respawned");
    set_translation(app.world_mut(), a, LID, Vec3::new(0.0, 7.0, 0.0));
    let lid_a = member(app.world_mut(), a, LID);
    let (choices, _) =
        crate::inspector::multi::prefab_apply_targets(app.world(), lid_a, "Transform", "translation");
    assert_eq!(
        choices.iter().map(|c| c.target).collect::<Vec<_>>(),
        vec![ApplyTarget::Variant]
    );
    let err = crate::inspector::multi::apply_prefab_override_to(
        app.world_mut(),
        lid_a,
        "Transform",
        "translation",
        Some(ApplyTarget::Base),
    )
    .expect_err("shadowed");
    assert_eq!(err.code, "apply_target_invalid");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Revert all is one undo step; a batch with a row that fails reverts the rows already applied
/// and leaves the files as they were.
#[test]
fn revert_all_is_one_step_and_a_failing_batch_rolls_back() {
    let (mut app, dir) = app_with_project("overrides-revert-all");
    let (inner, outer) = write_stack(&mut app, &dir);
    let outer_before = std::fs::read(&outer).expect("outer");
    let inner_before = std::fs::read(&inner).expect("inner");
    let one = crate::content::spawn_prefab_asset(app.world_mut(), &outer, Vec3::ZERO).expect("one");
    set_translation(app.world_mut(), one, LID, Vec3::new(0.0, 3.0, 0.0));
    set_translation(app.world_mut(), one, STACK_ROOT, Vec3::new(0.0, 0.0, 2.0));
    let lid = member(app.world_mut(), one, LID);
    let stack_root = member(app.world_mut(), one, STACK_ROOT);

    // The stack root cannot go to a nested target: the batch fails and the lid's Apply is undone.
    let depth = app.world().resource::<UndoStack>().undo_depth();
    let err = apply_rows(
        app.world_mut(),
        &[
            RowRef {
                entity: lid,
                component: "Transform".into(),
                path: "translation".into(),
                target: Some(ApplyTarget::Nested),
            },
            RowRef {
                entity: stack_root,
                component: "Transform".into(),
                path: "translation".into(),
                target: Some(ApplyTarget::Nested),
            },
        ],
    )
    .expect_err("second row has no nested target");
    assert_eq!(err.code, "apply_target_invalid");
    assert_eq!(std::fs::read(&inner).expect("inner"), inner_before);
    assert_eq!(std::fs::read(&outer).expect("outer"), outer_before);
    assert_eq!(app.world().resource::<UndoStack>().undo_depth(), depth);
    let list = override_rows(app.world(), one).expect("rows");
    assert_eq!(list.rows.iter().filter(|row| row.editable()).count(), 2);

    let rows = all_rows(app.world(), &list, None);
    let report = revert_rows(app.world_mut(), &rows).expect("revert all");
    assert_eq!(report.done, 2);
    assert_eq!(report.undo_depth, depth + 1, "one undo step");
    assert_eq!(translation(app.world_mut(), one, LID), Vec3::new(0.0, 1.0, 0.0));
    assert_eq!(translation(app.world_mut(), one, STACK_ROOT), Vec3::ZERO);
    assert!(undo_world(app.world_mut()));
    assert_eq!(translation(app.world_mut(), one, LID), Vec3::new(0.0, 3.0, 0.0));
    assert_eq!(
        translation(app.world_mut(), one, STACK_ROOT),
        Vec3::new(0.0, 0.0, 2.0)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The agent surface end to end through the dispatcher: rows, a batch apply with per-row
/// targets, the `target` of `editor.prefab_apply_override`, revert, and the error reasons.
#[test]
fn agent_methods_list_apply_and_revert() {
    let (mut app, dir) = app_with_project("overrides-agent");
    crate::agent_ext::install(app.world_mut());
    let (_inner, outer) = write_stack(&mut app, &dir);
    let one = crate::content::spawn_prefab_asset(app.world_mut(), &outer, Vec3::ZERO).expect("one");
    let two = crate::content::spawn_prefab_asset(app.world_mut(), &outer, Vec3::X).expect("two");
    set_translation(app.world_mut(), one, LID, Vec3::new(0.0, 4.0, 0.0));
    set_translation(app.world_mut(), one, STACK_ROOT, Vec3::new(0.0, 0.0, 5.0));
    let lid = member(app.world_mut(), one, LID);
    let call = |app: &mut App, method: &str, params: serde_json::Value| {
        let (_, handler) = app
            .world()
            .resource::<regia_agent::extension::AgentExtension>()
            .method(method)
            .unwrap_or_else(|| panic!("{method} registered"));
        handler(app.world_mut(), &params)
    };

    let listed = call(
        &mut app,
        "editor.prefab_overrides",
        json!({ "entity": lid.to_bits() }),
    )
    .expect("rows");
    assert_eq!(listed["root"], json!(one.to_bits()));
    let rows = listed["rows"].as_array().expect("rows");
    let lid_row = rows
        .iter()
        .find(|row| row["entity"] == json!(lid.to_bits()))
        .expect("lid row");
    assert_eq!(lid_row["kind"], json!("field"));
    assert_eq!(lid_row["default_target"], json!("outer"));
    assert_eq!(lid_row["targets"][0]["target"], json!("nested"));
    assert_eq!(
        lid_row["targets"][0]["file"],
        json!("assets/props/crate.prefab.ron")
    );

    // Unknown target name, and a target the row cannot take.
    let err = call(
        &mut app,
        "editor.prefab_apply_override",
        json!({ "entity": lid.to_bits(), "component": "Transform", "field": "translation", "target": "sideways" }),
    )
    .expect_err("unknown");
    assert_eq!(reason(&err).as_deref(), Some("apply_target_unknown"));
    let root_member = member(app.world_mut(), one, STACK_ROOT);
    let err = call(
        &mut app,
        "editor.prefab_apply_override",
        json!({ "entity": root_member.to_bits(), "component": "Transform", "field": "translation", "target": "nested" }),
    )
    .expect_err("no nested target");
    assert_eq!(reason(&err).as_deref(), Some("apply_target_invalid"));

    // The single-field method with a target.
    let reply = call(
        &mut app,
        "editor.prefab_apply_override",
        json!({ "entity": lid.to_bits(), "component": "Transform", "field": "translation", "target": "nested" }),
    )
    .expect("apply nested");
    assert_eq!(reply["file"], json!("assets/props/crate.prefab.ron"));
    assert_eq!(translation(app.world_mut(), two, LID).y, 4.0);

    // The batch method: every field row (the stack root's) to its default.
    let reply = call(
        &mut app,
        "editor.prefab_overrides_apply",
        json!({ "entity": one.to_bits() }),
    )
    .expect("apply all");
    assert_eq!(reply["applied"], json!(1));
    assert_eq!(reply["files"], json!(["assets/props/stack.prefab.ron"]));
    assert_eq!(translation(app.world_mut(), two, STACK_ROOT).z, 5.0);

    // Revert with explicit rows; nothing left to revert answers the reason.
    set_translation(app.world_mut(), one, LID, Vec3::new(0.0, 9.0, 0.0));
    let reply = call(
        &mut app,
        "editor.prefab_overrides_revert",
        json!({ "entity": one.to_bits(), "rows": [{ "entity": lid.to_bits(), "component": "Transform", "field": "translation" }] }),
    )
    .expect("revert");
    assert_eq!(reply["reverted"], json!(1));
    assert_eq!(translation(app.world_mut(), one, LID).y, 4.0);
    let err = call(
        &mut app,
        "editor.prefab_overrides_revert",
        json!({ "entity": one.to_bits() }),
    )
    .expect_err("nothing to revert");
    assert_eq!(reason(&err).as_deref(), Some("rows_empty"));
    let opened = call(
        &mut app,
        "editor.prefab_overrides_open",
        json!({ "entity": lid.to_bits() }),
    )
    .expect("open");
    assert_eq!(opened["open"], json!(true));
    assert!(app.world().resource::<PrefabOverridesWindow>().open);
    let err = call(
        &mut app,
        "editor.prefab_overrides",
        json!({ "entity": 999_999_u64 }),
    )
    .expect_err("missing entity");
    assert_eq!(err.code, regia_agent::protocol::NOT_FOUND);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Keyboard path (accessibility bar): Tab reaches the window's Apply all and Enter presses it.
#[test]
fn apply_all_is_reachable_from_the_keyboard() {
    use crate::canvas_keys::test_input::{frame, key, tab_to};
    let (mut app, dir) = app_with_project("overrides-keyboard");
    let (_inner, outer) = write_stack(&mut app, &dir);
    let one = crate::content::spawn_prefab_asset(app.world_mut(), &outer, Vec3::ZERO).expect("one");
    let two = crate::content::spawn_prefab_asset(app.world_mut(), &outer, Vec3::X).expect("two");
    set_translation(app.world_mut(), one, STACK_ROOT, Vec3::new(0.0, 0.0, 8.0));
    open_for(app.world_mut(), one);
    let ctx = egui::Context::default();
    let size = [800.0, 600.0];
    let world = app.world_mut();
    let mut draw = |ui: &mut egui::Ui| {
        egui::CentralPanel::default().show(ui, |ui| overrides_ui(ui, world));
    };
    frame(&ctx, size, vec![], &mut draw);
    let update = tab_to(&ctx, size, "Apply all", false, 40, &mut draw);
    assert!(
        crate::canvas_keys::test_input::labels_of(&update, egui::accesskit::Role::Button)
            .iter()
            .any(|label| label == "Apply Transform.translation"),
        "each row's Apply has a spoken name"
    );
    frame(
        &ctx,
        size,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
        &mut draw,
    );
    frame(&ctx, size, vec![], &mut draw);
    drop(draw);
    let world = app.world_mut();
    assert_eq!(translation(world, two, STACK_ROOT).z, 8.0, "Apply all ran");
    assert!(
        world
            .resource::<PrefabOverridesWindow>()
            .status
            .starts_with("Applied 1"),
        "{}",
        world.resource::<PrefabOverridesWindow>().status
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// P-160 Edit in place: the level stays as read-only context (not pickable, not part of the
/// prefab), the prefab's contents sit in the instance's frame, Save writes the prefab and
/// rebuilds every instance (the context ones too), and close restores the level.
#[test]
fn edit_in_place_keeps_the_level_as_context_and_close_restores_it() {
    let (mut app, dir) = app_with_project("overrides-in-place");
    crate::agent_ext::install(app.world_mut());
    let path = write_crate(&dir);
    let a = crate::content::spawn_prefab_asset(app.world_mut(), &path, Vec3::new(4.0, 0.0, 0.0)).expect("a");
    let b = crate::content::spawn_prefab_asset(app.world_mut(), &path, Vec3::new(-4.0, 0.0, 0.0)).expect("b");
    let plain = app
        .world_mut()
        .spawn((Transform::from_translation(Vec3::Y), EntityGuid::new_random()))
        .id();
    crate::visibility::ensure_snapshot_identity(app.world_mut());
    let a_guid = app.world().get::<EntityGuid>(a).expect("guid").0;
    let b_guid = app.world().get::<EntityGuid>(b).expect("guid").0;
    let level_path = app.world().resource::<crate::level_io::OpenLevel>().path.clone();
    let lid_a = member(app.world_mut(), a, LID);
    let (_, handler) = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .method("editor.prefab_edit_in_place")
        .expect("registered");
    let reply = handler(app.world_mut(), &json!({ "entity": lid_a.to_bits() })).expect("in place");
    assert_eq!(reply["in_context"], json!(true));
    assert_eq!(reply["editing"], json!("assets/props/crate.prefab.ron"));
    let (_, mode) = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .method("editor.prefab_mode")
        .expect("registered");
    let state = mode(app.world_mut(), &json!({})).expect("mode");
    assert_eq!(state["in_context"], json!(true), "{state}");

    let world = app.world_mut();
    // The level is still there, read-only.
    assert!(world.get_entity(plain).is_ok(), "the level stays drawn");
    assert!(world.get::<crate::prefab_edit::PrefabContext>(plain).is_some());
    assert!(
        crate::viewport_ops::is_locked(world, plain),
        "context is not pickable"
    );
    assert!(
        crate::visibility::is_hidden(world, plain),
        "context is out of the Outliner"
    );
    assert_eq!(
        world.get::<regia_scene::prelude::Visibility>(a).copied(),
        Some(regia_scene::prelude::Visibility::Hidden),
        "the instance opened in place is hidden"
    );
    // The prefab's contents: plain editable entities under a frame at the instance's transform.
    let edit_root = world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(entity, guid)| {
            guid.0 == Guid::from_u128(CRATE_ROOT) && world.get::<PrefabLink>(*entity).is_none()
        })
        .map(|(entity, _)| entity)
        .expect("the prefab root is editable");
    assert!(
        world
            .get::<crate::prefab_edit::PrefabContext>(edit_root)
            .is_none()
    );
    let frame = world
        .get::<ChildOf>(edit_root)
        .map(ChildOf::parent)
        .expect("framed");
    assert_eq!(
        world.get::<Transform>(frame).expect("frame").translation,
        Vec3::new(4.0, 0.0, 0.0)
    );
    let edit_lid = world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(_, guid)| guid.0 == Guid::from_u128(LID))
        .map(|(entity, _)| entity)
        .expect("lid");

    // Edit and save: the file has the new lid, one root without a parent, and every instance
    // (both in the context) is rebuilt and stays context.
    inspector::edit_value(
        world,
        &[edit_lid],
        "Transform",
        "translation",
        Box::new(Vec3::new(0.0, 2.5, 0.0)),
    )
    .expect("edit");
    let updated = crate::prefab_edit::save_prefab(world).expect("save");
    assert_eq!(updated, 2, "both instances update");
    let saved = std::fs::read_to_string(&path).expect("saved");
    let file = SceneFile::from_ron_str(&saved).expect("parse");
    assert_eq!(file.entities.len(), 2, "only the prefab's own entities: {saved}");
    assert_eq!(
        file.entities
            .iter()
            .filter(|record| record.parent.is_none())
            .count(),
        1
    );
    assert!(record_component(&saved, LID).contains("2.5"), "{saved}");
    let live_b = world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(_, guid)| guid.0 == b_guid)
        .map(|(entity, _)| entity)
        .expect("b");
    assert_eq!(
        translation(world, live_b, LID).y,
        2.5,
        "a context instance updates"
    );
    assert!(
        world.get::<crate::prefab_edit::PrefabContext>(live_b).is_some(),
        "and stays read-only context"
    );

    // Close: the context goes, the level comes back from the stash with its path.
    crate::prefab_edit::exit_prefab_edit(world, false).expect("close");
    assert_eq!(
        world
            .query_filtered::<Entity, With<crate::prefab_edit::PrefabContext>>()
            .iter(world)
            .count(),
        0
    );
    assert_eq!(world.resource::<crate::level_io::OpenLevel>().path, level_path);
    let live_a = world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(_, guid)| guid.0 == a_guid)
        .map(|(entity, _)| entity)
        .expect("a restored");
    assert_ne!(
        world.get::<regia_scene::prelude::Visibility>(live_a).copied(),
        Some(regia_scene::prelude::Visibility::Hidden),
        "the instance shows again"
    );
    assert_eq!(
        translation(world, live_a, LID).y,
        2.5,
        "restored from the saved prefab"
    );
    assert!(
        !crate::viewport_ops::is_locked(world, live_a),
        "the level is pickable again"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A variant cannot be opened in place (variant edit mode is out of scope, spec 83 § 9), and a
/// plain entity is no instance.
#[test]
fn edit_in_place_refuses_variants_and_plain_entities() {
    let (mut app, dir) = app_with_project("overrides-in-place-variant");
    crate::agent_ext::install(app.world_mut());
    let base = write_crate(&dir);
    let variant = dir.join("assets/props/crate_tall.variant.ron");
    crate::prefab_variant_edit::create_prefab_variant(app.world_mut(), &base, &variant, None)
        .expect("variant");
    let a = crate::content::spawn_prefab_asset(app.world_mut(), &variant, Vec3::ZERO).expect("a");
    let (_, handler) = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .method("editor.prefab_edit_in_place")
        .expect("registered");
    let err = handler(app.world_mut(), &json!({ "entity": a.to_bits() })).expect_err("variant");
    assert_eq!(reason(&err).as_deref(), Some("prefab_variant_in_place"));
    assert!(!crate::prefab_edit::in_prefab_mode(app.world()));
    let plain = app.world_mut().spawn(Transform::default()).id();
    let err = handler(app.world_mut(), &json!({ "entity": plain.to_bits() })).expect_err("plain");
    assert_eq!(reason(&err).as_deref(), Some("not_prefab_instance"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review finding 3 (rev-g5): a variant that nests the prefab and overrides the field in its edit
/// list shadows the nested prefab's file, so Nested is not offered and an Apply to it is refused
/// (it would write the inner file while the variant's value still wins on this instance). The
/// same variant without the override still offers Nested.
#[test]
fn a_variant_that_overrides_a_nested_field_takes_nested_out_of_the_choices() {
    let (mut app, dir) = app_with_project("overrides-variant-nested-shadow");
    let (inner, outer) = write_stack(&mut app, &dir);
    let variant = dir.join("assets/props/stack_tall.variant.ron");
    crate::prefab_variant_edit::create_prefab_variant(app.world_mut(), &outer, &variant, None)
        .expect("variant");
    let a = crate::content::spawn_prefab_asset(app.world_mut(), &variant, Vec3::ZERO).expect("a");
    set_translation(app.world_mut(), a, LID, Vec3::new(0.0, 3.0, 0.0));
    let lid = member(app.world_mut(), a, LID);
    let (choices, _) =
        crate::inspector::multi::prefab_apply_targets(app.world(), lid, "Transform", "translation");
    assert!(
        choices
            .iter()
            .any(|c| c.target == ApplyTarget::Nested && c.file == inner),
        "no variant override yet: the nested file is a target ({choices:?})"
    );

    // The variant overrides the lid in its edit list.
    crate::prefab_variant_edit::set_variant_override(
        app.world_mut(),
        &variant,
        Guid::from_u128(LID),
        "regia_scene::transform::Transform",
        "translation",
        Some("(0.0, 2.0, 0.0)"),
    )
    .expect("variant override");
    let b = crate::content::spawn_prefab_asset(app.world_mut(), &variant, Vec3::X).expect("b");
    assert_eq!(
        translation(app.world_mut(), b, LID).y,
        2.0,
        "the variant's override reaches the nested lid"
    );
    let a = crate::command::resolve_entity(app.world(), a.to_bits()).expect("a respawned");
    set_translation(app.world_mut(), a, LID, Vec3::new(0.0, 7.0, 0.0));
    let lid = member(app.world_mut(), a, LID);
    let (choices, _) =
        crate::inspector::multi::prefab_apply_targets(app.world(), lid, "Transform", "translation");
    assert!(
        choices.iter().all(|c| c.target != ApplyTarget::Nested),
        "the variant shadows the nested file: {choices:?}"
    );
    let inner_before = std::fs::read(&inner).expect("inner");
    let err = crate::inspector::multi::apply_prefab_override_to(
        app.world_mut(),
        lid,
        "Transform",
        "translation",
        Some(ApplyTarget::Nested),
    )
    .expect_err("shadowed");
    assert_eq!(err.code, "apply_target_invalid");
    assert_eq!(
        std::fs::read(&inner).expect("inner"),
        inner_before,
        "nothing written"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review finding 1 (rev-g5): in Edit in place the prefab's contents are pickable in the viewport
/// and listed in the Outliner (as roots: the frame they hang under is not listed), while the level
/// around them stays locked and out of the Outliner.
#[test]
fn edit_in_place_contents_are_pickable_and_listed_in_the_outliner() {
    let (mut app, dir) = app_with_project("overrides-in-place-pick");
    let path = write_crate(&dir);
    let a = crate::content::spawn_prefab_asset(app.world_mut(), &path, Vec3::new(4.0, 0.0, 0.0)).expect("a");
    let plain = app
        .world_mut()
        .spawn((Transform::from_translation(Vec3::Y), EntityGuid::new_random()))
        .id();
    let lid = member(app.world_mut(), a, LID);
    crate::prefab_edit::enter_prefab_edit_in_context(app.world_mut(), lid).expect("in place");
    let world = app.world_mut();
    let edit = |world: &mut World, local: u128| {
        world
            .query::<(Entity, &EntityGuid)>()
            .iter(world)
            .find(|(entity, guid)| {
                guid.0 == Guid::from_u128(local) && world.get::<PrefabLink>(*entity).is_none()
            })
            .map(|(entity, _)| entity)
            .expect("editable prefab entity")
    };
    let edit_root = edit(world, CRATE_ROOT);
    let edit_lid = edit(world, LID);
    for entity in [edit_root, edit_lid] {
        assert!(
            !crate::viewport_ops::is_locked(world, entity),
            "the opened prefab is not locked"
        );
        assert!(
            crate::viewport_ops::pickable(world, entity),
            "the opened prefab is pickable"
        );
    }
    let frame = world
        .get::<ChildOf>(edit_root)
        .map(ChildOf::parent)
        .expect("framed");
    assert!(crate::prefab_edit::is_in_place_frame(world, frame));
    assert!(
        crate::visibility::is_hidden(world, frame),
        "the frame is no Outliner row"
    );
    let tree = crate::outliner::hierarchy_tree(world);
    let root_row = tree
        .iter()
        .find(|node| node.entity == edit_root)
        .expect("the prefab's root is an Outliner root");
    assert!(
        root_row.children.iter().any(|node| node.entity == edit_lid),
        "with its children"
    );
    assert!(
        tree.iter()
            .all(|node| node.entity != frame && node.entity != plain)
    );
    assert!(
        crate::viewport_ops::is_locked(world, plain),
        "the level stays locked"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review finding 2 (rev-g5): while a prefab is open in place, a level save is refused (the
/// shared save path and `editor.save_level`, with or without `path`): nothing is written, the
/// level file keeps its bytes, and the prefab's unsaved edits stay dirty. After close, the level
/// saves again and the instance opened in place is not hidden in the file.
#[test]
fn a_level_save_is_refused_while_a_prefab_is_open_in_place() {
    let (mut app, dir) = app_with_project("overrides-in-place-save");
    crate::agent_ext::install(app.world_mut());
    let path = write_crate(&dir);
    let a = crate::content::spawn_prefab_asset(app.world_mut(), &path, Vec3::new(4.0, 0.0, 0.0)).expect("a");
    let level = dir.join("assets/arena.level.ron");
    let mut open = app.world().resource::<crate::level_io::OpenLevel>().clone();
    open.path = Some(level.clone());
    app.world_mut().insert_resource(open.clone());
    crate::level_io::save_level_and_terrain(app.world_mut(), &open, &level).expect("first save");
    let level_before = std::fs::read(&level).expect("level");
    let copy = dir.join("assets/copy.level.ron");

    let lid = member(app.world_mut(), a, LID);
    crate::prefab_edit::enter_prefab_edit_in_context(app.world_mut(), lid).expect("in place");
    let world = app.world_mut();
    let edit_lid = world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(entity, guid)| guid.0 == Guid::from_u128(LID) && world.get::<PrefabLink>(*entity).is_none())
        .map(|(entity, _)| entity)
        .expect("lid");
    inspector::edit_value(
        world,
        &[edit_lid],
        "Transform",
        "translation",
        Box::new(Vec3::new(0.0, 2.5, 0.0)),
    )
    .expect("edit");
    assert!(crate::prefab_edit::is_dirty(world));

    // The shared save path (File ▸ Save level / Save level as, the close modal, Save all).
    let open_now = world.resource::<crate::level_io::OpenLevel>().clone();
    let err = crate::level_io::save_level_and_terrain(world, &open_now, &level).expect_err("refused");
    assert_eq!(err, crate::prefab_edit::LEVEL_SAVE_IN_PLACE);
    // The agent method, with and without a path.
    let (_, save) = world
        .resource::<regia_agent::extension::AgentExtension>()
        .method("editor.save_level")
        .expect("registered");
    for params in [
        json!({ "path": "assets/arena.level.ron" }),
        json!({ "path": "assets/copy.level.ron" }),
        json!({}),
    ] {
        let err = save(world, &params).expect_err("refused");
        assert_eq!(reason(&err).as_deref(), Some("prefab_in_place"), "{params}");
    }
    assert_eq!(
        std::fs::read(&level).expect("level"),
        level_before,
        "the level file is untouched"
    );
    assert!(!copy.exists(), "no save-as copy");
    assert!(
        crate::prefab_edit::is_dirty(world),
        "the prefab's edits are still unsaved"
    );

    // Close: the level saves again, and the instance is not hidden in the file.
    crate::prefab_edit::exit_prefab_edit(world, false).expect("close");
    let reply = save(world, &json!({})).expect("saves after close");
    assert!(reply.is_object(), "{reply}");
    let text = std::fs::read_to_string(&level).expect("level");
    assert!(!text.contains("Hidden"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}
