// SPDX-License-Identifier: MPL-2.0
//! P-165 (ADR-8566): prefab edit mode's Open graph / New graph for this prefab.

use std::path::{Path, PathBuf};

use regia_app::prelude::App;

use super::*;
use crate::canvas_keys::test_input::{frame, key, tab_to};
use crate::command::UndoStack;
use crate::editor_layout::{LayoutRequest, LayoutRequests};
use crate::shell::Panel;

const SIZE: [f32; 2] = [900.0, 200.0];

fn project() -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir("prefab-logic");
    std::fs::create_dir_all(dir.join("assets/props")).expect("dir");
    dir
}

/// A prefab file: one root with a child.
fn write_prefab(dir: &Path, stem: &str) -> PathBuf {
    let path = dir.join("assets/props").join(format!("{stem}.prefab.ron"));
    let text = r#"( format_version: 1, entities: [
        ( guid: "6c1e2a11-0000-4000-8000-000000000001", name: "Root",
          components: { "regia_scene::transform::Transform": () } ),
        ( guid: "6c1e2a11-0000-4000-8000-000000000002", name: "Hinge", parent: Some("6c1e2a11-0000-4000-8000-000000000001"),
          components: { "regia_scene::transform::Transform": ( translation: (0.0, 1.0, 0.0) ) } ),
    ] )"#;
    std::fs::write(&path, text).expect("prefab");
    path
}

/// An edit world with `door.prefab.ron` open in prefab edit mode.
fn editing(dir: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut().insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.to_path_buf()),
    });
    let open = crate::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    app.insert_resource(UndoStack::new());
    let prefab = write_prefab(dir, "door");
    crate::prefab_edit::enter_prefab_edit(app.world_mut(), &prefab).expect("prefab mode");
    app
}

fn root(world: &mut World) -> Entity {
    crate::prefab_edit::prefab_root(world).expect("prefab root")
}

fn depth(world: &World) -> usize {
    world.resource::<UndoStack>().undo_depth()
}

#[test]
fn new_graph_for_this_prefab_creates_and_attaches_in_one_undo_step() {
    let dir = project();
    let mut app = editing(&dir);
    let world = app.world_mut();
    let before = depth(world);
    let done = prefab_logic(world, PrefabLogicOp::New, false).expect("new graph");
    assert!(done.created);
    assert_eq!(done.path, "assets/props/door.logic.ron");
    assert_eq!(done.file, dir.join("assets/props/door.logic.ron"));
    // The file is an empty, valid graph.
    let text = std::fs::read_to_string(&done.file).expect("graph file");
    LogicGraphDef::from_ron(&text).expect("a .logic.ron");
    // The root holds the reference: one undo step.
    let root = root(world);
    assert_eq!(done.entity, root.to_bits());
    assert_eq!(
        world.get::<LogicGraphRef>(root).map(|r| r.graph.as_str()),
        Some("assets/props/door.logic.ron")
    );
    assert_eq!(depth(world), before + 1, "one undo step");
    assert_eq!(done.undoable, before + 1);
    // The graph is open in the Logic Graph panel, which comes forward.
    assert_eq!(
        world.resource::<LogicEditor>().path.as_deref(),
        Some(done.file.as_path())
    );
    assert!(
        world
            .resource::<LayoutRequests>()
            .pending
            .contains(&LayoutRequest::Open(Panel::LogicGraph))
    );
    // Undo takes the component off and leaves the file; redo puts it back.
    assert!(crate::command::undo_world(world));
    assert!(world.get::<LogicGraphRef>(root).is_none());
    assert!(done.file.is_file(), "the file stays on undo");
    assert_eq!(depth(world), before);
    assert!(crate::command::redo_world(world));
    assert!(world.get::<LogicGraphRef>(root).is_some());
    // A save writes the reference into the prefab: every instance carries it.
    crate::prefab_edit::save_prefab(world).expect("save");
    let saved = std::fs::read_to_string(dir.join("assets/props/door.prefab.ron")).expect("prefab text");
    assert!(
        saved.contains("LogicGraphRef") && saved.contains("assets/props/door.logic.ron"),
        "{saved}"
    );
    // A second New on the same root is refused; after undo the next name is free.
    let err = prefab_logic(world, PrefabLogicOp::New, true).expect_err("the root has a graph");
    assert_eq!(err.code, "prefab_logic_exists");
    assert!(crate::command::undo_world(world));
    let again = prefab_logic(world, PrefabLogicOp::New, true).expect("new graph again");
    assert_eq!(again.path, "assets/props/door_2.logic.ron");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_graph_opens_what_the_prefab_references() {
    let dir = project();
    let mut app = editing(&dir);
    let world = app.world_mut();
    let err = prefab_logic(world, PrefabLogicOp::Open, false).expect_err("no reference yet");
    assert_eq!(err.code, "prefab_logic_none");
    let made = prefab_logic(world, PrefabLogicOp::New, false).expect("new graph");
    world.insert_resource(LogicEditor::default());
    world.insert_resource(LayoutRequests::default());
    let before = depth(world);
    let done = prefab_logic(world, PrefabLogicOp::Open, false).expect("open");
    assert!(!done.created);
    assert_eq!(
        (done.path.as_str(), done.entity),
        (made.path.as_str(), made.entity)
    );
    assert_eq!(
        world.resource::<LogicEditor>().path.as_deref(),
        Some(made.file.as_path())
    );
    assert_eq!(depth(world), before, "opening is no edit");
    assert!(
        world
            .resource::<LayoutRequests>()
            .pending
            .contains(&LayoutRequest::Open(Panel::LogicGraph))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unsaved_logic_work_is_kept_unless_discarded() {
    let dir = project();
    let mut app = editing(&dir);
    let world = app.world_mut();
    let made = prefab_logic(world, PrefabLogicOp::New, false).expect("new graph");
    // Unsaved work in the panel: another graph opened and edited.
    let other = dir.join("assets/props/other.logic.ron");
    std::fs::write(&other, LogicGraphDef::new().to_ron().expect("ron")).expect("other graph");
    {
        let mut editor = world.resource_mut::<LogicEditor>();
        editor.open(&other).expect("open other");
        editor.doc.description = "draft".into();
    }
    assert!(world.resource::<LogicEditor>().has_unsaved_work());
    let err = prefab_logic(world, PrefabLogicOp::Open, false).expect_err("unsaved work");
    assert_eq!(err.code, "prefab_logic_unsaved");
    assert_eq!(
        world.resource::<LogicEditor>().path.as_deref(),
        Some(other.as_path()),
        "the draft stays open"
    );
    let done = prefab_logic(world, PrefabLogicOp::Open, true).expect("discard");
    assert!(done.discarded_unsaved);
    assert_eq!(
        world.resource::<LogicEditor>().path.as_deref(),
        Some(made.file.as_path())
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn outside_prefab_mode_both_actions_are_refused() {
    let dir = project();
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut().insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    for op in [PrefabLogicOp::Open, PrefabLogicOp::New] {
        let err = prefab_logic(app.world_mut(), op, false).expect_err("not editing");
        assert_eq!(err.code, "prefab_logic_not_editing");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_graph_names_follow_the_prefab_and_count_up() {
    assert_eq!(
        new_graph_path("assets/props/door.prefab.ron", |_| false),
        "assets/props/door.logic.ron"
    );
    let taken = ["assets/props/door.logic.ron", "assets/props/door_2.logic.ron"];
    assert_eq!(
        new_graph_path("assets/props/door.prefab.ron", |c| taken.contains(&c)),
        "assets/props/door_3.logic.ron"
    );
    assert_eq!(new_graph_path("gate.prefab.ron", |_| false), "gate.logic.ron");
    assert_eq!(new_graph_path("a/odd.ron", |_| false), "a/odd.logic.ron");
}

/// The keyboard path (accessibility bar): Tab reaches the banner's New graph for this prefab and
/// Enter runs it; Tab then reaches Open graph.
#[test]
fn the_banner_buttons_work_from_the_keyboard() {
    let dir = project();
    let mut app = editing(&dir);
    let world = app.world_mut();
    let ctx = egui::Context::default();
    let mut statuses: Vec<String> = Vec::new();
    let mut draw = |ui: &mut egui::Ui| {
        ui.horizontal(|ui| {
            if let Some(status) = banner_buttons(ui, world) {
                statuses.push(status);
            }
        });
    };
    frame(&ctx, SIZE, vec![], &mut draw);
    tab_to(&ctx, SIZE, "New graph for this prefab", false, 20, &mut draw);
    frame(
        &ctx,
        SIZE,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
        &mut draw,
    );
    frame(&ctx, SIZE, vec![], &mut draw);
    tab_to(&ctx, SIZE, "Open graph", false, 20, &mut draw);
    frame(
        &ctx,
        SIZE,
        vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
        &mut draw,
    );
    frame(&ctx, SIZE, vec![], &mut draw);
    drop(draw);
    assert_eq!(
        statuses,
        vec![
            "new logic graph assets/props/door.logic.ron on the prefab's root".to_string(),
            "opened assets/props/door.logic.ron".to_string(),
        ]
    );
    let root = root(app.world_mut());
    assert!(app.world().get::<LogicGraphRef>(root).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}
