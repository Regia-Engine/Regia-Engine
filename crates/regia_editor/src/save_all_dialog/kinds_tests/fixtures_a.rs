// SPDX-License-Identifier: MPL-2.0
//! Save all fixtures (see `super::Fixture`).

use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::MethodSchema;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::Fixture;

/// Calls the agent method `method` from an editor's method table.
fn call<H>(methods: Vec<(MethodSchema, H)>, world: &mut World, method: &str, params: Value) -> Value
where
    H: Fn(&mut World, &Value) -> Result<Value, RpcError>,
{
    let (_, handler) = methods
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("no method {method}"));
    handler(world, &params).unwrap_or_else(|e| panic!("{method}: {e:?}"))
}

/// Marks the shared undo stack saved: an asset editor's edit lands on it, and the level is not
/// what these fixtures test (an unsaved level would add its own row).
fn level_clean(world: &mut World) {
    if let Some(mut stack) = world.get_resource_mut::<crate::command::UndoStack>() {
        stack.mark_saved();
    }
}

/// A material graph opened in the material graph editor, with one node added.
fn matgraph(app: &mut regia_app::App, root: &Path) -> PathBuf {
    let rel = crate::content::create_asset(
        root,
        crate::content::NewAssetKind::MaterialGraph,
        "fixture.matgraph.ron",
    )
    .expect("create the material graph");
    let file = root.join(rel);
    let mut editor = crate::matgraph_editor::MatGraphEditor::default();
    editor.open(&file).expect("open the material graph");
    editor
        .doc
        .add_node(Some("save_all_probe"), regia_matgraph::Node::Float(0.5), None)
        .expect("add a node");
    app.world_mut().insert_resource(editor);
    file
}

/// The UI designer's test layout, with a comment (the save merges into it).
const UI_DOC: &str = r#"// a comment
(
    version: 2,
    root: (
        style: (direction: column),
        children: [
            (name: "title", text: (spans: ["Hello"])),
            (name: "row", children: [
                (name: "ok", widget: button, text: (spans: ["OK"])),
                (name: "cancel", widget: button),
            ]),
        ],
    ),
)"#;

/// A `.ui.ron` opened in the UI designer, with one field set.
fn ui_design(app: &mut regia_app::App, root: &Path) -> PathBuf {
    let file = root.join("assets/fixture.ui.ron");
    std::fs::write(&file, UI_DOC).expect("write the layout");
    let mut designer = crate::ui_designer::UiDesigner::default();
    designer.open(&file).expect("open the layout");
    designer
        .doc_mut()
        .expect("open document")
        .apply(crate::ui_designer::DesignOp::Set {
            node: vec![0],
            key: "command".into(),
            value: json!("menu.title"),
        })
        .expect("set a field");
    app.world_mut().insert_resource(designer);
    file
}

/// A blank procedural graph created and opened in the procedural editor, with one node moved.
fn procgen(app: &mut regia_app::App, root: &Path) -> PathBuf {
    let world = app.world_mut();
    world.insert_resource(regia_procgen::world::ProcGenSettings {
        editor: true,
        source: regia_procgen::world::ProcGraphSource::Dir(root.to_path_buf()),
        ..regia_procgen::world::ProcGenSettings::default()
    });
    crate::procgen::install(world);
    let rel = "assets/fixture.pgraph.ron";
    crate::procgen::doc::new_graph(world, rel, None).expect("create the graph");
    crate::procgen::doc::edit(world, "move node", None, |ctx| {
        crate::procgen::doc::move_node(ctx.graph, 1, (40.0, 40.0))
    })
    .expect("move a node");
    level_clean(world);
    root.join(rel)
}

/// A `.bt.ron` opened in the behavior tree editor, with a root node added.
fn behavior_tree(app: &mut regia_app::App, root: &Path) -> PathBuf {
    let rel = "assets/fixture.bt.ron";
    let file = root.join(rel);
    std::fs::write(&file, "(root: 0, nodes: [])").expect("write the tree");
    let world = app.world_mut();
    call(
        crate::bt_editor::agent::methods(),
        world,
        "editor.bt_open",
        json!({ "path": rel }),
    );
    call(
        crate::bt_editor::agent::methods(),
        world,
        "editor.bt_edit",
        json!({ "ops": [{ "op": "add_node", "kind": "selector" }] }),
    );
    level_clean(world);
    file
}

/// A `.statetree.ron` opened in the state tree editor, built into a valid guard.
fn state_tree(app: &mut regia_app::App, root: &Path) -> PathBuf {
    let rel = "assets/fixture.statetree.ron";
    let file = root.join(rel);
    std::fs::write(
        &file,
        r#"(format_version: 2, root: "Root", states: [(name: "Root")])"#,
    )
    .expect("write the tree");
    let world = app.world_mut();
    call(
        crate::state_tree_editor::agent::methods(),
        world,
        "editor.statetree_open",
        json!({ "path": rel }),
    );
    let built = call(
        crate::state_tree_editor::agent::methods(),
        world,
        "editor.statetree_edit",
        json!({ "label": "build", "ops": [
            { "op": "add_state", "name": "Idle", "parent": "Root" },
            { "op": "add_state", "name": "Alert", "parent": "Root" },
            { "op": "add_transition", "state": "Idle", "trigger": { "Event": "heard" }, "target": { "State": "Alert" } },
            { "op": "set_param", "state": "Alert", "key": "speed", "value": { "Float": 2.5 } },
        ] }),
    );
    assert_eq!(
        built["valid"], true,
        "the state tree saves only when valid: {built}"
    );
    level_clean(world);
    file
}

/// The Tables panel's test curve table.
const CURVES: &str = r#"(
    format_version: 1,
    interp: Linear,
    curves: {
        "falloff": (keys: [(0.0, 1.0), (10.0, 0.6), (30.0, 0.0)]),
        "xp": (interp: Cubic, post: Linear, keys: [(1, 0), (2, 100), (3, 250)]),
    },
)"#;

/// A curve table opened in the Tables panel, with one key added.
fn table(app: &mut regia_app::App, root: &Path) -> PathBuf {
    let file = root.join("assets/tables/fixture.curves.ron");
    std::fs::create_dir_all(root.join("assets/tables")).expect("tables dir");
    std::fs::write(&file, CURVES).expect("write the table");
    let world = app.world_mut();
    call(
        crate::tables_panel::methods(),
        world,
        "editor.tables_panel_open",
        json!({ "path": "tables/fixture.curves.ron" }),
    );
    call(
        crate::tables_panel::methods(),
        world,
        "editor.tables_panel_curve_edit",
        json!({ "op": "set_key", "name": "falloff", "x": 20.0, "y": 0.5 }),
    );
    level_clean(world);
    file
}

/// A new sound cue opened in the cue editor, with one variation added.
fn sound_cue(app: &mut regia_app::App, root: &Path) -> PathBuf {
    let rel = crate::content::create_asset(root, crate::content::NewAssetKind::SoundCue, "fixture.cue.ron")
        .expect("create the cue");
    let world = app.world_mut();
    crate::cue_editor_panel::open_cue(world, &rel).expect("open the cue");
    let changed = crate::cue_editor_panel::edit_cue(
        world,
        &crate::cue_editor_panel::CueChange::AddSound("audio/step_a.wav".into(), 1.0),
    )
    .expect("add a variation");
    assert!(changed, "the edit changes the cue");
    level_clean(world);
    root.join(rel)
}

/// The fixtures of this file.
pub(super) fn fixtures() -> Vec<(&'static str, Fixture)> {
    vec![
        ("matgraph", matgraph as Fixture),
        ("ui_design", ui_design as Fixture),
        ("procgen", procgen as Fixture),
        ("behavior_tree", behavior_tree as Fixture),
        ("state_tree", state_tree as Fixture),
        ("table", table as Fixture),
        ("sound_cue", sound_cue as Fixture),
    ]
}
