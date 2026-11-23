// SPDX-License-Identifier: MPL-2.0
use super::*;
use regia_ai::state_tree::{Target, Trigger};
use regia_app::prelude::{App, MinimalPlugins};

/// The fixture file, in the canonical layout (so saving it unchanged gives the same bytes).
const GUARD: &str = r#"(
    format_version: 2,
    root: "Root",
    states: [
        (
            name: "Root",
            children: ["Idle", "Alert"],
        ),
        (
            name: "Idle",
            transitions: [
                (trigger: Event("heard"), target: State("Alert")),
            ],
        ),
        (
            name: "Alert",
            transitions: [
                (trigger: Event("calm"), target: State("Idle")),
            ],
        ),
    ],
)
"#;

fn add(name: &str, parent: &str) -> StOp {
    StOp::AddState {
        name: name.into(),
        parent: Some(parent.into()),
        index: None,
    }
}

fn rename(name: &str, to: &str) -> StOp {
    StOp::RenameState {
        name: name.into(),
        to: to.into(),
    }
}

fn edit_world() -> World {
    let mut world = World::new();
    world.insert_resource(UndoStack::new());
    world
}

fn fixture(label: &str) -> (PathBuf, PathBuf) {
    let dir = regia_test_utils::unique_temp_dir(label);
    let file = dir.join("guard.statetree.ron");
    std::fs::write(&file, GUARD).expect("writes the fixture");
    (dir, file)
}

fn open(world: &mut World, file: &Path) {
    world
        .get_resource_or_insert_with(StEditor::default)
        .open(file)
        .expect("opens");
}

#[test]
fn edits_undo_and_redo_through_the_shared_stack_and_the_selection_follows_a_rename() {
    let mut world = edit_world();
    world
        .get_resource_or_insert_with(StEditor::default)
        .new_tree(None, "Root");
    edit(&mut world, "add", None, &[add("Idle", "Root")]).expect("adds");
    assert_eq!(world.resource::<StEditor>().selected.as_deref(), Some("Idle"));
    edit(&mut world, "rename", None, &[rename("Idle", "Calm")]).expect("renames");
    assert_eq!(world.resource::<StEditor>().selected.as_deref(), Some("Calm"));
    assert_eq!(world.resource::<StEditor>().doc.states[0].children, ["Calm"]);
    crate::command::undo_world(&mut world);
    assert_eq!(world.resource::<StEditor>().doc.states[0].children, ["Idle"]);
    assert_eq!(world.resource::<StEditor>().selected.as_deref(), Some("Idle"));
    crate::command::undo_world(&mut world);
    assert_eq!(world.resource::<StEditor>().doc.states.len(), 1);
    crate::command::redo_world(&mut world);
    crate::command::redo_world(&mut world);
    assert_eq!(world.resource::<StEditor>().doc.states[1].name, "Calm");
    // A refused batch adds no step.
    let depth = world.resource::<UndoStack>().undo_depth();
    let err = edit(&mut world, "bad", None, &[add("X", "Nope")]).expect_err("refused");
    assert_eq!(err.error.code, "unknown_state");
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth);
    // Same-key edits merge, and an old session's undo does nothing.
    for w in [1.5_f32, 2.0, 2.5] {
        let op = StOp::SetState {
            name: "Calm".into(),
            selection: None,
            enter: None,
            weight: Some(w),
            utility: None,
            clear_utility: false,
            fallback: None,
        };
        edit(&mut world, "weight", Some("w:Calm".into()), &[op]).expect("sets");
    }
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth + 1);
    world.resource_mut::<StEditor>().new_tree(None, "Other");
    crate::command::undo_world(&mut world);
    assert_eq!(world.resource::<StEditor>().doc.states[0].name, "Other");
}

#[test]
fn undo_restores_the_file_bytes() {
    let (dir, file) = fixture("st-editor-undo-bytes");
    let mut world = edit_world();
    open(&mut world, &file);
    let original = std::fs::read(&file).expect("reads");
    edit(&mut world, "rename", None, &[rename("Alert", "Alarm")]).expect("renames");
    save(&mut world, None).expect("saves");
    let renamed = std::fs::read(&file).expect("reads");
    assert_ne!(renamed, original);
    assert_eq!(
        String::from_utf8_lossy(&renamed),
        GUARD.replace("\"Alert\"", "\"Alarm\""),
        "the rename reached every reference"
    );
    assert!(!world.resource::<StEditor>().dirty());
    crate::command::undo_world(&mut world);
    assert!(world.resource::<StEditor>().dirty());
    save(&mut world, None).expect("saves");
    assert_eq!(
        std::fs::read(&file).expect("reads"),
        original,
        "undo then save gives back the bytes"
    );
    assert!(!dir.join("guard.statetree.ron.new").exists() && !dir.join("guard.statetree.new").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_tree_with_errors_is_not_saved_and_the_file_stays() {
    let (dir, file) = fixture("st-editor-invalid");
    let mut world = edit_world();
    open(&mut world, &file);
    let original = std::fs::read(&file).expect("reads");
    let op = StOp::AddTransition {
        state: "Idle".into(),
        trigger: Trigger::Tick,
        condition: None,
        target: Target::State("Ghost".into()),
        priority: None,
        hold: None,
        index: None,
    };
    edit(&mut world, "dangling", None, &[op]).expect("the edit itself applies");
    let issues = diagnose(&world);
    assert_eq!(issues[0].code, "missing_target");
    assert_eq!(issues[0].path, "states/Idle/transitions/1/target");
    let err = save(&mut world, None).expect_err("refused");
    assert_eq!(err.code, "invalid_tree");
    assert_eq!(std::fs::read(&file).expect("reads"), original);
    assert!(world.resource::<StEditor>().dirty());
    let err = save(&mut world, Some(&dir.join("x.ron"))).expect_err("wrong extension");
    assert_eq!(err.code, "bad_path");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A play world running `n` agents from `file` (each with its own compiled tree and a
/// `StateTreeSource` naming the file), stepped until the trees started.
fn play_world(file: &Path, n: usize) -> (App, Vec<Entity>) {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_ai::AiPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    let def = state_tree_edit::parse_statetree_ron(GUARD).expect("parses");
    let registry = BtTaskRegistry::default();
    let mut agents = Vec::new();
    for _ in 0..n {
        let tree = StateTree::compile(&def, &registry).expect("compiles");
        let e = app
            .world_mut()
            .spawn((
                regia_ai::Blackboard::default(),
                tree,
                StateTreeSource {
                    path: file.to_path_buf(),
                },
            ))
            .id();
        agents.push(e);
    }
    for _ in 0..3 {
        app.update();
    }
    (app, agents)
}

fn frame(edit: &mut World, play: &mut App) {
    pump_world(edit, Some(play.world_mut()));
    play.update();
    play.update();
    pump_world(edit, Some(play.world_mut()));
}

#[test]
fn saving_while_playing_reloads_the_running_tree_kept_then_restarted() {
    let (dir, file) = fixture("st-editor-pie-reload");
    let (mut play, agents) = play_world(&file, 1);
    let mut edit_w = edit_world();
    open(&mut edit_w, &file);
    pump_world(&mut edit_w, Some(play.world_mut()));
    {
        let editor = edit_w.resource::<StEditor>();
        let live = editor.live.clone().expect("a live view while playing");
        assert_eq!(live.active_path, ["Root", "Idle"], "the active path to highlight");
        assert!(live.uses_file && live.matches_document);
        assert_eq!(editor.agents, [agents[0].to_bits()]);
    }
    // A new state: every active state survives, so the tree keeps its path.
    edit(&mut edit_w, "add", None, &[add("Search", "Root")]).expect("adds");
    save(&mut edit_w, None).expect("saves");
    frame(&mut edit_w, &mut play);
    let reload = edit_w
        .resource::<StEditor>()
        .reload
        .clone()
        .expect("a reload view");
    assert_eq!(
        (
            reload.save,
            reload.delivered,
            reload.kept,
            reload.restarted,
            reload.last
        ),
        (1, true, 1, 0, Some("kept"))
    );
    assert_eq!(
        play.world()
            .get::<StateTree>(agents[0])
            .map(|t| t.def().states.len()),
        Some(4)
    );
    let live = edit_w.resource::<StEditor>().live.clone().expect("live");
    assert_eq!(live.active_path, ["Root", "Idle"]);
    assert!(live.matches_document, "the running tree is the saved document");
    // Remove the active state: the tree restarts from the root.
    let remove = StOp::RemoveState {
        name: "Idle".into(),
        subtree: false,
    };
    edit(&mut edit_w, "remove", None, &[remove]).expect("removes");
    save(&mut edit_w, None).expect("saves");
    frame(&mut edit_w, &mut play);
    let reload = edit_w
        .resource::<StEditor>()
        .reload
        .clone()
        .expect("a reload view");
    assert_eq!(
        (reload.save, reload.kept, reload.restarted, reload.last),
        (2, 0, 1, Some("restarted"))
    );
    let live = edit_w.resource::<StEditor>().live.clone().expect("live");
    assert_eq!(
        live.active_path,
        ["Root", "Alert"],
        "restarted into the first child left"
    );
    // Play stops: the live view clears, the last reload stays readable.
    pump_world(&mut edit_w, None);
    assert!(edit_w.resource::<StEditor>().live.is_none());
    assert!(edit_w.resource::<StEditor>().reload.is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_hundred_agents_sharing_the_file_compile_it_once_per_save() {
    let (dir, file) = fixture("st-editor-hundred");
    let (mut play, agents) = play_world(&file, 100);
    let mut edit_w = edit_world();
    open(&mut edit_w, &file);
    for save_no in 1..=2_u64 {
        let name = format!("Extra{save_no}");
        edit(&mut edit_w, "add", None, &[add(&name, "Root")]).expect("adds");
        save(&mut edit_w, None).expect("saves");
        frame(&mut edit_w, &mut play);
        let reloads = play.world().resource::<StateTreeReloads>();
        assert_eq!(reloads.file_compiles, save_no, "one compile per save");
        assert_eq!(reloads.instances, 100 * save_no);
        let view = edit_w.resource::<StEditor>().reload.clone().expect("view");
        assert_eq!(view.restarted + view.rejected, 0);
        // The log keeps the last records; every one of them is a kept reload of this save.
        assert_eq!(view.kept, regia_ai::state_tree::MAX_RELOAD_LOG as u64);
    }
    for e in agents {
        assert_eq!(
            play.world().get::<StateTree>(e).map(|t| t.def().states.len()),
            Some(5)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn draw_frames(world: &mut World) -> usize {
    let ctx = egui::Context::default();
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(900.0, 700.0),
        )),
        ..Default::default()
    };
    let mut output = ctx.run_ui(raw, |ui| panel::state_tree_editor_panel(ui, world));
    output.textures_delta.clear();
    ctx.tessellate(output.shapes, output.pixels_per_point).len()
}

#[test]
fn the_panel_draws_headless_in_every_state() {
    let mut world = edit_world();
    assert!(draw_frames(&mut world) > 0, "nothing open");
    let (dir, file) = fixture("st-editor-draw");
    open(&mut world, &file);
    world.resource_mut::<StEditor>().selected = Some("Idle".into());
    assert!(draw_frames(&mut world) > 0, "a tree with a selection");
    edit(
        &mut world,
        "stray",
        None,
        &[StOp::AddState {
            name: "Stray".into(),
            parent: None,
            index: None,
        }],
    )
    .expect("adds");
    assert!(draw_frames(&mut world) > 0, "a tree with errors");
    world.resource_mut::<StEditor>().live = Some(LiveView {
        entity: 1,
        status: "running",
        active_path: vec!["Root".into(), "Idle".into()],
        ticks: 3,
        uses_file: true,
        matches_document: false,
    });
    world.resource_mut::<StEditor>().reload = Some(ReloadView {
        save: 1,
        delivered: true,
        kept: 1,
        last: Some("kept"),
        ..ReloadView::default()
    });
    assert!(draw_frames(&mut world) > 0, "while playing");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn conditions_typed_as_ron_parse_or_are_refused() {
    use regia_ai::state_tree::{CompareOp, Condition};
    assert_eq!(panel::parse_condition("Always"), Some(Condition::Always));
    assert_eq!(
        panel::parse_condition(" Compare(key: \"dist\", op: Lt, value: 12.0) "),
        Some(Condition::Compare {
            key: "dist".into(),
            op: CompareOp::Lt,
            value: 12.0,
        })
    );
    assert_eq!(
        panel::parse_condition("All([Key(\"a\"), Not(Has(\"b\"))])").map(|c| c.depth()),
        Some(3)
    );
    assert_eq!(panel::parse_condition("Key("), None);
    assert_eq!(
        panel::parse_condition("Compare(key: \"d\", op: Lt, value: inf)"),
        None
    );
}

/// FU-5: the States list under the node view selects a state (Enter), moves it among its
/// siblings (Alt+Up) and removes another (Delete) with keys only, through the same ops the mouse
/// and inspector use; the document equals the one those ops build directly.
#[test]
fn the_states_list_edits_by_keyboard_what_the_mouse_edits() {
    use crate::canvas_keys::test_input::{focused_name, frame, key, labels_of, tab_to};
    use egui::{Key, Modifiers};
    const SIZE: [f32; 2] = [1200.0, 1400.0];
    let start = |world: &mut World| {
        world
            .get_resource_or_insert_with(StEditor::default)
            .new_tree(None, "Root");
        edit(world, "add", None, &[add("A", "Root"), add("B", "Root")]).expect("adds");
    };
    let mut mouse = edit_world();
    start(&mut mouse);
    edit(
        &mut mouse,
        "move B",
        None,
        &[StOp::ReparentState {
            name: "B".into(),
            parent: "Root".into(),
            index: Some(0),
        }],
    )
    .expect("moves");
    edit(
        &mut mouse,
        "remove A",
        None,
        &[StOp::RemoveState {
            name: "A".into(),
            subtree: false,
        }],
    )
    .expect("removes");

    let mut world = edit_world();
    start(&mut world);
    world.resource_mut::<StEditor>().selected = None;
    let ctx = egui::Context::default();
    let draw = |world: &mut World, events: Vec<egui::Event>| {
        frame(&ctx, SIZE, events, |ui| {
            super::panel::state_tree_editor_panel(ui, world)
        })
    };
    let first = draw(&mut world, vec![]);
    assert!(
        labels_of(&first, egui::accesskit::Role::Canvas).contains(&"State tree canvas, 3 states".to_string()),
        "{:?}",
        labels_of(&first, egui::accesskit::Role::Canvas)
    );
    tab_to(&ctx, SIZE, "B, under Root, level 1", false, 400, |ui| {
        super::panel::state_tree_editor_panel(ui, &mut world)
    });
    draw(&mut world, vec![key(Key::Enter, Modifiers::NONE)]);
    assert_eq!(
        world.resource::<StEditor>().selected.as_deref(),
        Some("B"),
        "Enter selects"
    );
    let depth = world.resource::<UndoStack>().undo_depth();
    draw(&mut world, vec![key(Key::ArrowUp, Modifiers::ALT)]);
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth + 1,
        "one entry per press"
    );
    assert_eq!(world.resource::<StEditor>().doc.states[0].children, ["B", "A"]);
    draw(&mut world, vec![]);
    let moved = draw(&mut world, vec![]);
    assert_eq!(
        focused_name(&moved).as_deref(),
        Some("B, under Root, level 1"),
        "the focus followed B"
    );
    draw(&mut world, vec![key(Key::ArrowDown, Modifiers::NONE)]);
    draw(&mut world, vec![]);
    draw(&mut world, vec![key(Key::Delete, Modifiers::NONE)]);
    let ron =
        |world: &World| state_tree_edit::to_canonical_ron(&world.resource::<StEditor>().doc).expect("ron");
    assert_eq!(ron(&world), ron(&mouse));
}

#[test]
fn keyboard_moves_stop_at_the_edges_of_the_tree() {
    let mut world = edit_world();
    world
        .get_resource_or_insert_with(StEditor::default)
        .new_tree(None, "Root");
    edit(
        &mut world,
        "add",
        None,
        &[add("A", "Root"), add("B", "Root"), add("C", "B")],
    )
    .expect("adds");
    let def = world.resource::<StEditor>().doc.clone();
    let mv = |name: &str, dx, dy| super::panel::keyboard_move(&def, name, dx, dy);
    assert_eq!(mv("Root", 0, 1), None, "the root has no siblings");
    assert_eq!(mv("A", 0, -1), None, "the first sibling cannot go up");
    assert_eq!(mv("B", 0, 1), None, "the last sibling cannot go down");
    assert_eq!(mv("A", -1, 0), None, "a child of the root has no grandparent");
    assert_eq!(
        mv("A", 1, 0),
        None,
        "the first sibling has no previous sibling to go under"
    );
    assert_eq!(
        mv("C", -1, 0),
        Some(StOp::ReparentState {
            name: "C".into(),
            parent: "Root".into(),
            index: Some(2)
        }),
        "out: after its parent"
    );
    assert_eq!(
        mv("B", 1, 0),
        Some(StOp::ReparentState {
            name: "B".into(),
            parent: "A".into(),
            index: None
        }),
        "in: under the previous sibling"
    );
}
