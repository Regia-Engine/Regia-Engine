// SPDX-License-Identifier: MPL-2.0
use super::*;
use crate::level_io::{self, OpenLevel};
use regia_app::prelude::App;
use regia_scene::placed::PlacedMember;
use regia_test_utils::unique_temp_dir;

const SUB: Guid = Guid::from_u128(0xe0);
const SOURCE: Guid = Guid::from_u128(0x10);
const INSTANCE: Guid = Guid::from_u128(0x30);

fn record(guid: Guid, extra: &str) -> String {
    format!(
        r#"(guid: "{guid}", components: {{ "regia_scene::transform::Transform": (translation: (1.0, 2.0, 3.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), {extra} }}),"#
    )
}

fn level_text(records: &str) -> String {
    format!(
        "SceneFile(format_version: {}, entities: [{records}], resources: {{}})",
        regia_scene::format::SCENE_FORMAT_VERSION
    )
}

/// A project whose host level places `sub.level.ron` inline once, opened in the editor.
fn host_app(name: &str) -> (App, std::path::PathBuf, std::path::PathBuf) {
    host_app_with(
        name,
        &record(
            INSTANCE,
            &format!(r#""regia_scene::placed::PlacedLevel": (level: "asset:{SUB}"),"#),
        ),
    )
}

/// The group the parented fixture puts the instance under, and the instance's sibling after it.
const GROUP: Guid = Guid::from_u128(0x20);
const SIBLING: Guid = Guid::from_u128(0x40);

/// [`host_app`] with the instance under a group, before a sibling: `GROUP { INSTANCE, SIBLING }`.
fn parented_host_app(name: &str) -> (App, std::path::PathBuf, std::path::PathBuf) {
    let child = |guid: Guid, extra: &str| {
        format!(
            r#"(guid: "{guid}", parent: Some("{GROUP}"), components: {{ "regia_scene::transform::Transform": (translation: (1.0, 2.0, 3.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), {extra} }}),"#
        )
    };
    let records = format!(
        "{}{}{}",
        record(GROUP, ""),
        child(
            INSTANCE,
            &format!(r#""regia_scene::placed::PlacedLevel": (level: "asset:{SUB}"),"#)
        ),
        child(SIBLING, ""),
    );
    host_app_with(name, &records)
}

/// A project whose host level holds `host_records`, with `sub.level.ron` to place, opened in the
/// editor.
fn host_app_with(name: &str, host_records: &str) -> (App, std::path::PathBuf, std::path::PathBuf) {
    let root = unique_temp_dir(name);
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    let sub = assets.join("sub.level.ron");
    std::fs::write(&sub, level_text(&record(SOURCE, ""))).expect("sub");
    let sidecar = regia_asset::MetaSidecar::new("level", regia_asset::AssetId(SUB));
    std::fs::write(
        regia_asset::meta_path_for(&sub),
        sidecar.to_ron_string().expect("meta"),
    )
    .expect("meta");
    let host = assets.join("host.level.ron");
    std::fs::write(&host, level_text(host_records)).expect("host");
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    crate::mesh_source::register_level_types(app.world_mut());
    app.world_mut().insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    app.world_mut().insert_resource(UndoStack::new());
    let open = level_io::open_level_file(app.world_mut(), &host).expect("open host");
    app.world_mut().insert_resource(open);
    app.update();
    (app, root, host)
}

fn saved(app: &mut App, path: &std::path::Path) -> Vec<u8> {
    let open = app.world().resource::<OpenLevel>().clone();
    level_io::save_level_file(app.world_mut(), &open, path).expect("save");
    std::fs::read(path).expect("read")
}

fn call(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (_, handler) = methods()
        .into_iter()
        .find(|(m, _)| m.name == "editor.break_placed")
        .expect("registered");
    handler(world, params)
}

/// Spec 37 § 6.5: Break is one undo step and the undo restores the level bytes exactly; a redo
/// gives the broken level's bytes again.
#[test]
fn break_is_one_undo_step_and_undo_restores_the_level_bytes() {
    let (mut app, root, _host) = host_app("placed_break_undo");
    let out_path = root.join("out.level.ron");
    let instance = crate::layers_panel::entity_of(app.world_mut(), INSTANCE).expect("instance");
    let member = regia_scene::placed::derive_guid(INSTANCE, SOURCE);
    assert!(
        crate::layers_panel::entity_of(app.world_mut(), member).is_some(),
        "the inline fill spawned the member"
    );
    let before = saved(&mut app, &out_path);
    let depth = app.world().resource::<UndoStack>().undo_depth();

    let out = call(app.world_mut(), &json!({ "entity": instance.to_bits() })).expect("break");
    assert_eq!(out["entities"], json!([member.to_string()]));
    assert_eq!(app.world().resource::<UndoStack>().undo_depth(), depth + 1);
    let broken_member = crate::layers_panel::entity_of(app.world_mut(), member).expect("member stays");
    assert!(app.world().get::<PlacedMember>(broken_member).is_none());
    assert!(crate::layers_panel::entity_of(app.world_mut(), INSTANCE).is_none());
    app.update();
    let broken = saved(&mut app, &out_path);
    assert_ne!(broken, before);
    let text = String::from_utf8_lossy(&broken).to_string();
    assert!(text.contains(&member.to_string()), "{text}");
    assert!(!text.contains("PlacedLevel"), "{text}");

    assert!(crate::command::undo_world(app.world_mut()));
    app.update();
    assert_eq!(
        String::from_utf8_lossy(&saved(&mut app, &out_path)),
        String::from_utf8_lossy(&before),
        "undo restores the level bytes"
    );
    let member_again = crate::layers_panel::entity_of(app.world_mut(), member).expect("refilled");
    assert!(app.world().get::<PlacedMember>(member_again).is_some());

    assert!(crate::command::redo_world(app.world_mut()));
    app.update();
    assert_eq!(saved(&mut app, &out_path), broken, "redo breaks again");
    let _ = std::fs::remove_dir_all(root);
}

/// Moves `entity` to `x` as one sealed undo step (the gizmo's command).
fn move_to(app: &mut App, entity: Entity, x: f32) -> crate::command::SetTransform {
    let world = app.world_mut();
    let mut new = *world
        .get::<regia_scene::prelude::Transform>(entity)
        .expect("transform");
    new.translation.x = x;
    let mut command = crate::command::SetTransform::capture(world, entity, new);
    command.apply(world);
    let copy = crate::command::SetTransform {
        entity: command.entity,
        old: command.old,
        new: command.new,
    };
    let mut stack = world.resource_mut::<UndoStack>();
    stack.push(Box::new(command));
    stack.seal();
    app.update();
    copy
}

/// rev-claude-64 (3) finding 1: Break of an instance under a group, then undo, gives the level's
/// bytes back (the instance returns under its group, at its place before its sibling), and the
/// undo entries around the Break still find their entities: an older move of the instance undoes,
/// and a move of a former member redoes after the members were refilled and broken again.
#[test]
fn break_of_a_parented_instance_undoes_to_the_same_bytes_and_keeps_the_history_live() {
    let (mut app, root, _host) = parented_host_app("placed_break_parented");
    let out_path = root.join("out.level.ron");
    let instance = crate::layers_panel::entity_of(app.world_mut(), INSTANCE).expect("instance");
    let group = crate::layers_panel::entity_of(app.world_mut(), GROUP).expect("group");
    assert_eq!(
        app.world().get::<ChildOf>(instance).map(ChildOf::parent),
        Some(group),
        "the fixture parents the instance"
    );
    let opened = saved(&mut app, &out_path);
    let opened_text = String::from_utf8_lossy(&opened).to_string();
    assert!(
        opened_text.matches(&GROUP.to_string()).count() >= 3,
        "the group's record and its two children's parents name it: {opened_text}"
    );

    // An edit of the instance before the Break.
    move_to(&mut app, instance, 7.0);
    let moved = saved(&mut app, &out_path);
    assert_ne!(moved, opened);

    call(app.world_mut(), &json!({ "entity": instance.to_bits() })).expect("break");
    app.update();
    assert!(crate::command::undo_world(app.world_mut()), "undo break");
    app.update();
    assert_eq!(
        String::from_utf8_lossy(&saved(&mut app, &out_path)),
        String::from_utf8_lossy(&moved),
        "undo of the Break restores the level bytes, the instance's parent and place included"
    );
    assert!(crate::command::undo_world(app.world_mut()), "undo move");
    app.update();
    assert_eq!(
        String::from_utf8_lossy(&saved(&mut app, &out_path)),
        String::from_utf8_lossy(&opened),
        "the move recorded before the Break still undoes on the restored instance"
    );

    // Redo both, then move a former member, undo the move and the Break, redo both.
    assert!(crate::command::redo_world(app.world_mut()), "redo move");
    assert!(crate::command::redo_world(app.world_mut()), "redo break");
    app.update();
    let member_guid = regia_scene::placed::derive_guid(INSTANCE, SOURCE);
    let member = crate::layers_panel::entity_of(app.world_mut(), member_guid).expect("member");
    let member_move = move_to(&mut app, member, 5.0);
    let member_moved = saved(&mut app, &out_path);
    assert!(crate::command::undo_world(app.world_mut()), "undo member move");
    assert!(crate::command::undo_world(app.world_mut()), "undo break again");
    app.update();
    assert!(crate::command::redo_world(app.world_mut()), "redo break again");
    assert!(crate::command::redo_world(app.world_mut()), "redo member move");
    app.update();
    let member = crate::layers_panel::entity_of(app.world_mut(), member_guid).expect("member");
    assert_eq!(
        app.world()
            .get::<regia_scene::prelude::Transform>(member)
            .copied(),
        Some(member_move.new),
        "the member's move redoes on the refilled member"
    );
    assert_eq!(
        String::from_utf8_lossy(&saved(&mut app, &out_path)),
        String::from_utf8_lossy(&member_moved)
    );
    let _ = std::fs::remove_dir_all(root);
}

/// The refusals: a plain entity is `not_placed`, a missing entity `placed_not_found`.
#[test]
fn break_refuses_what_is_not_an_inline_instance() {
    let (mut app, root, _host) = host_app("placed_break_refusals");
    let plain = app.world_mut().spawn(EntityGuid(Guid::from_u128(0x77))).id();
    let err = call(app.world_mut(), &json!({ "entity": plain.to_bits() })).expect_err("plain");
    assert_eq!(err.data.expect("data")["code"], "not_placed");
    app.world_mut().despawn(plain);
    let err = call(app.world_mut(), &json!({ "entity": plain.to_bits() })).expect_err("gone");
    assert_eq!(err.data.expect("data")["code"], "placed_not_found");
    assert!(call(app.world_mut(), &json!({})).is_err());
    let _ = std::fs::remove_dir_all(root);
}

/// The Outliner's row menu leaves Break in egui memory; `outliner_frame` runs it as the same one
/// undo step. The Layers panel draws its Edit / Break row for a selected instance, and the
/// definition lists are scanned once, not per frame.
#[test]
fn the_outliner_runs_a_row_menu_break_and_the_panel_draws_the_instance_row() {
    let (mut app, root, _host) = host_app("placed_break_outliner");
    let instance = crate::layers_panel::entity_of(app.world_mut(), INSTANCE).expect("instance");
    let mut selection = crate::selection::Selection::default();
    selection.select(instance, crate::selection::SelectMode::Replace);
    app.world_mut().insert_resource(selection);
    let ctx = egui::Context::default();
    for _ in 0..2 {
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            crate::layers_panel::layers_panel(ui, app.world_mut());
        });
        output.textures_delta.clear();
    }
    let form = app
        .world()
        .get_resource::<crate::layers_panel::LayerEditForm>()
        .cloned()
        .expect("the form");
    assert!(form.scanned, "scanned once and kept");

    let depth = app.world().resource::<UndoStack>().undo_depth();
    ctx.data_mut(|d| {
        d.insert_temp(
            egui::Id::new(super::PENDING_ROW_EDIT),
            crate::layers_panel::LayerEdit::BreakPlaced(instance),
        );
    });
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        outliner_frame(ui.ctx(), app.world_mut());
    });
    output.textures_delta.clear();
    assert!(
        crate::layers_panel::entity_of(app.world_mut(), INSTANCE).is_none(),
        "broken"
    );
    assert_eq!(app.world().resource::<UndoStack>().undo_depth(), depth + 1);
    assert!(
        ctx.data(|d| d.get_temp::<crate::layers_panel::LayerEdit>(egui::Id::new(super::PENDING_ROW_EDIT)))
            .is_none(),
        "the edit ran once"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// A non-member child of the placed-level instance, authored in the host level.
const LAMP: Guid = Guid::from_u128(0x50);
/// A non-member child the user put under a member after the level opened.
const SIGN: Guid = Guid::from_u128(0x60);

/// The entities with `guid` (a respawn must not leave two).
fn all_with(world: &mut World, guid: Guid) -> Vec<Entity> {
    let mut query = world.query::<(Entity, &EntityGuid)>();
    query
        .iter(world)
        .filter(|(_, g)| g.0 == guid)
        .map(|(e, _)| e)
        .collect()
}

fn parent_of(world: &World, entity: Entity) -> Option<Entity> {
    world.get::<ChildOf>(entity).map(ChildOf::parent)
}

/// Lane v06-editor-usability-a item 2 (w11 "Break undo re-roots non-member children"): Break keeps
/// the entities that are not members of the placed level (a lamp the host level put under the
/// instance, a sign the user put under a member) and its undo puts each back under its old parent
/// (the restored instance, the refilled member), once, with its old local transform. Before the
/// fix the Break's despawn of the instance deleted the lamp, and the undo brought the sign back
/// as a scene root (its parent, a former member, was not refilled yet when it respawned).
#[test]
fn break_and_its_undo_keep_non_member_children_under_their_parents() {
    let instance_record = record(
        INSTANCE,
        &format!(r#""regia_scene::placed::PlacedLevel": (level: "asset:{SUB}"),"#),
    );
    let lamp_record = format!(
        r#"(guid: "{LAMP}", parent: Some("{INSTANCE}"), components: {{ "regia_scene::transform::Transform": (translation: (0.5, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)) }}),"#
    );
    let records = format!("{instance_record}{lamp_record}");
    let (mut app, root, _host) = host_app_with("placed_break_non_members", &records);
    let out_path = root.join("out.level.ron");
    let instance = crate::layers_panel::entity_of(app.world_mut(), INSTANCE).expect("instance");
    let lamp = crate::layers_panel::entity_of(app.world_mut(), LAMP).expect("lamp");
    assert_eq!(
        parent_of(app.world(), lamp),
        Some(instance),
        "the fixture parents the lamp"
    );
    let lamp_local = *app
        .world()
        .get::<regia_scene::prelude::Transform>(lamp)
        .expect("lamp transform");
    let member_guid = regia_scene::placed::derive_guid(INSTANCE, SOURCE);
    let member = crate::layers_panel::entity_of(app.world_mut(), member_guid).expect("member");
    let level = app
        .world()
        .get::<LevelMember>(member)
        .copied()
        .expect("member level");
    let sign_local =
        regia_scene::prelude::Transform::from_translation(regia_core::math::Vec3::new(0.0, 1.0, 0.0));
    let sign = app
        .world_mut()
        .spawn((EntityGuid(SIGN), sign_local, level, ChildOf(member)))
        .id();
    app.update();
    let before = saved(&mut app, &out_path);

    call(app.world_mut(), &json!({ "entity": instance.to_bits() })).expect("break");
    app.update();
    let lamps = all_with(app.world_mut(), LAMP);
    assert_eq!(lamps.len(), 1, "Break keeps the instance's non-member child");
    assert_eq!(
        parent_of(app.world(), lamps[0]),
        None,
        "it goes where the instance was (the root)"
    );
    let lamp_world = app
        .world()
        .get::<regia_scene::prelude::Transform>(lamps[0])
        .expect("lamp transform")
        .translation;
    assert!(
        (lamp_world - regia_core::math::Vec3::new(1.5, 2.0, 3.0)).length() < 1e-5,
        "and keeps its world place: {lamp_world:?}"
    );
    let member = crate::layers_panel::entity_of(app.world_mut(), member_guid).expect("member stays");
    assert_eq!(
        parent_of(app.world(), sign),
        Some(member),
        "the sign stays under the member"
    );

    assert!(crate::command::undo_world(app.world_mut()), "undo break");
    app.update();
    let instance = crate::layers_panel::entity_of(app.world_mut(), INSTANCE).expect("instance back");
    let member = crate::layers_panel::entity_of(app.world_mut(), member_guid).expect("member refilled");
    let lamps = all_with(app.world_mut(), LAMP);
    let signs = all_with(app.world_mut(), SIGN);
    assert_eq!(
        (lamps.len(), signs.len()),
        (1, 1),
        "each non-member is there once"
    );
    assert_eq!(
        parent_of(app.world(), lamps[0]),
        Some(instance),
        "the lamp is under the instance again"
    );
    assert_eq!(
        app.world()
            .get::<regia_scene::prelude::Transform>(lamps[0])
            .copied(),
        Some(lamp_local),
        "with its old local transform"
    );
    assert_eq!(
        parent_of(app.world(), signs[0]),
        Some(member),
        "the undo puts the sign back under the refilled member, not at the scene root"
    );
    assert_eq!(
        app.world()
            .get::<regia_scene::prelude::Transform>(signs[0])
            .copied(),
        Some(sign_local)
    );
    assert_eq!(
        String::from_utf8_lossy(&saved(&mut app, &out_path)),
        String::from_utf8_lossy(&before),
        "undo restores the level bytes"
    );

    assert!(crate::command::redo_world(app.world_mut()), "redo break");
    app.update();
    let lamps = all_with(app.world_mut(), LAMP);
    let signs = all_with(app.world_mut(), SIGN);
    assert_eq!((lamps.len(), signs.len()), (1, 1));
    assert_eq!(parent_of(app.world(), lamps[0]), None);
    let member = crate::layers_panel::entity_of(app.world_mut(), member_guid).expect("member");
    assert_eq!(parent_of(app.world(), signs[0]), Some(member));
    let _ = std::fs::remove_dir_all(root);
}
