// SPDX-License-Identifier: MPL-2.0
//! P-322 part b: the Theme panel, the UI Animation editor, the UI designer and the Vehicle Tuning
//! panel's trailer and aircraft documents keep their edits on the editor's one shared history
//! (`command::UndoStack`), and document autosave sees the documents a panel keeps retained behind
//! the open one (`book/agents/undo-redo.md`, `book/agents/editor-autosave.md`).
//!
//! Every call goes through the real dispatcher, fed from an in-process inbox, so each call is
//! answered within one deterministic frame (no wall-clock waits).

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::level_io;
use regia_editor::selection::Selection;
use serde_json::{Value, json};

/// An agent talking to the edit world through the dispatcher.
struct Agent {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
}

impl Agent {
    fn new(project: &std::path::Path) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.to_path_buf()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.init_resource::<UndoStack>()
            .init_resource::<Selection>()
            .init_resource::<regia_editor::viewport::EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<regia_editor::viewport::ViewportSettings>();
        let open = level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        regia_editor::agent_ext::install(app.world_mut());
        app.world_mut()
            .insert_resource(regia_editor::agent_ext::EditorProject {
                root: Some(project.to_path_buf()),
            });
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded::<Inbound>();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        app.world_mut().insert_resource(permissions);
        Self {
            app,
            inbox,
            next_id: 0,
        }
    }

    fn try_call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next_id += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(self.next_id)),
            method: method.into(),
            params,
        };
        self.inbox.send(Inbound { request, reply }).expect("inbox open");
        self.app.update();
        let response = answer
            .try_recv()
            .expect("the dispatcher answers within the frame");
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(value), None) => Ok(value),
            (None, None) => panic!("{method}: empty response"),
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        match self.try_call(method, params.clone()) {
            Ok(value) => value,
            Err(err) => panic!("{method} {params}: {err:?}"),
        }
    }

    fn depth(&mut self) -> u64 {
        self.call("editor.history", json!({}))["undo_depth"]
            .as_u64()
            .expect("depth")
    }
}

fn project(name: &str) -> std::path::PathBuf {
    let dir = regia_test_utils::unique_temp_dir(name);
    std::fs::create_dir_all(dir.join("assets")).expect("assets");
    dir
}

/// The draft's own value of `key`, if it sets one.
fn theme_token(agent: &Agent, key: &str) -> Option<String> {
    agent
        .app
        .world()
        .get_resource::<regia_editor::theme_panel::ThemePanel>()
        .and_then(|panel| panel.draft())
        .and_then(|draft| draft.tokens.get(key).map(|value| format!("{value:?}")))
}

#[test]
fn an_edit_in_theme_panel_undoes_from_the_global_stack() {
    let dir = project("undo-global-theme");
    let mut agent = Agent::new(&dir);
    agent.call(
        "editor.theme_panel_open",
        json!({"new": {"name": "undo-global", "extends": "regia-dark"}}),
    );
    let base = agent.depth();
    agent.call(
        "editor.theme_panel_set",
        json!({"tokens": {"color.accent": "#ff0000"}}),
    );
    assert!(theme_token(&agent, "color.accent").is_some(), "the edit applied");
    assert_eq!(
        agent.depth(),
        base + 1,
        "the edit is one step on the shared stack"
    );
    agent.call("editor.undo", json!({}));
    assert_eq!(
        theme_token(&agent, "color.accent"),
        None,
        "editor.undo reverts it"
    );
    agent.call("editor.redo", json!({}));
    assert!(
        theme_token(&agent, "color.accent").is_some(),
        "editor.redo reapplies it"
    );
    // The panel's own history route is the shared one too.
    agent.call("editor.theme_panel_set", json!({"history": "undo"}));
    assert_eq!(theme_token(&agent, "color.accent"), None);
    assert_eq!(agent.depth(), base);
    let _ = std::fs::remove_dir_all(dir);
}

fn animations(agent: &Agent) -> usize {
    agent
        .app
        .world()
        .get_resource::<regia_editor::ui_anim_doc::UiAnimEditor>()
        .and_then(|editor| editor.doc())
        .map_or(0, |doc| doc.library().animations.len())
}

#[test]
fn an_edit_in_ui_anim_undoes_from_the_global_stack() {
    let dir = project("undo-global-ui-anim");
    let mut agent = Agent::new(&dir);
    agent.call("editor.ui_anim_open", json!({"new": true}));
    let base = agent.depth();
    agent.call(
        "editor.ui_anim_edit",
        json!({"op": "add_animation", "name": "fade_in"}),
    );
    assert_eq!(animations(&agent), 1);
    assert_eq!(
        agent.depth(),
        base + 1,
        "the edit is one step on the shared stack"
    );
    agent.call("editor.undo", json!({}));
    assert_eq!(animations(&agent), 0, "editor.undo reverts it");
    agent.call("editor.redo", json!({}));
    assert_eq!(animations(&agent), 1, "editor.redo reapplies it");
    let listed = agent.call("editor.ui_anim_edit", json!({"op": "undo"}));
    assert_eq!(animations(&agent), 0, "the editor's own undo is the shared one");
    assert_eq!(
        listed["animations"].as_array().map(Vec::len),
        Some(0),
        "the answer shows the undone state: {listed}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

fn designer_children(agent: &Agent) -> usize {
    agent
        .app
        .world()
        .get_resource::<regia_editor::ui_designer::UiDesigner>()
        .and_then(|designer| designer.doc())
        .map_or(0, |doc| doc.file().root.children.len())
}

#[test]
fn an_edit_in_ui_designer_undoes_from_the_global_stack() {
    let dir = project("undo-global-ui-designer");
    let mut agent = Agent::new(&dir);
    agent.call("editor.ui_designer_open", json!({"new": true}));
    let base = agent.depth();
    let start = designer_children(&agent);
    agent.call(
        "editor.ui_designer_edit",
        json!({"op": "add", "preset": "button", "parent": "/"}),
    );
    assert_eq!(designer_children(&agent), start + 1);
    assert_eq!(
        agent.depth(),
        base + 1,
        "the edit is one step on the shared stack"
    );
    agent.call("editor.undo", json!({}));
    assert_eq!(designer_children(&agent), start, "editor.undo reverts it");
    agent.call("editor.redo", json!({}));
    assert_eq!(designer_children(&agent), start + 1, "editor.redo reapplies it");
    let state = agent.call("editor.ui_designer_edit", json!({"op": "undo"}));
    assert_eq!(
        designer_children(&agent),
        start,
        "the designer's own undo is the shared one"
    );
    assert_eq!(
        state["undo"],
        json!(base),
        "the answer counts the shared steps: {state}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_designers_drag_of_three_nodes_is_one_step() {
    let dir = project("undo-global-ui-designer-drag");
    let mut agent = Agent::new(&dir);
    agent.call("editor.ui_designer_open", json!({"new": true}));
    for preset in ["panel", "text", "button"] {
        agent.call(
            "editor.ui_designer_edit",
            json!({"op": "add", "preset": preset, "parent": "/"}),
        );
    }
    let opacity = |agent: &Agent| -> Vec<Option<f32>> {
        let designer = agent
            .app
            .world()
            .resource::<regia_editor::ui_designer::UiDesigner>();
        let doc = designer.doc().expect("doc");
        doc.file().root.children.iter().map(|c| c.opacity).collect()
    };
    let before = opacity(&agent);
    let base = agent.depth();
    let nodes = vec![vec![0], vec![1], vec![2]];
    // A drag on the shared opacity field of three selected nodes: one set per frame.
    for value in [0.9, 0.7, 0.5] {
        {
            let world = agent.app.world_mut();
            let mut designer = world.resource_mut::<regia_editor::ui_designer::UiDesigner>();
            let doc = designer.doc_mut().expect("doc");
            doc.set_many(&nodes, "opacity", &json!(value), true).expect("set");
        }
        agent.app.update();
    }
    {
        let world = agent.app.world_mut();
        let mut designer = world.resource_mut::<regia_editor::ui_designer::UiDesigner>();
        designer.doc_mut().expect("doc").end_merge();
    }
    agent.app.update();
    assert_eq!(opacity(&agent), vec![Some(0.5); 3]);
    assert_eq!(agent.depth(), base + 1, "the whole drag is one shared step");
    agent.call("editor.undo", json!({}));
    assert_eq!(opacity(&agent), before, "one undo restores all three nodes");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_edit_in_vehicle_trailer_and_aircraft_undoes_from_the_global_stack() {
    let dir = project("undo-global-vehicle-others");
    let mut agent = Agent::new(&dir);
    agent.call(
        "editor.vehicle_tuning_open",
        json!({"path": "assets/t.trailer.ron", "create": true}),
    );
    agent.call(
        "editor.vehicle_tuning_open",
        json!({"path": "assets/a.aircraft.ron", "create": true}),
    );
    let base = agent.depth();
    agent.call(
        "editor.vehicle_tuning_set",
        json!({"asset": "trailer", "field": "name", "value": "flatbed"}),
    );
    agent.call(
        "editor.vehicle_tuning_set",
        json!({"asset": "aircraft", "field": "name", "value": "glider"}),
    );
    assert_eq!(
        agent.depth(),
        base + 2,
        "each edit is one step on the shared stack"
    );
    let name = |agent: &mut Agent, kind: &str| -> Value {
        agent.call("editor.vehicle_tuning_state", json!({}))[kind]["name"].clone()
    };
    assert_eq!(name(&mut agent, "aircraft"), "glider");
    agent.call("editor.undo", json!({}));
    assert_ne!(
        name(&mut agent, "aircraft"),
        "glider",
        "editor.undo reverts the aircraft"
    );
    assert_eq!(name(&mut agent, "trailer"), "flatbed");
    agent.call("editor.undo", json!({}));
    assert_ne!(name(&mut agent, "trailer"), "flatbed", "then the trailer");
    agent.call("editor.redo", json!({}));
    assert_eq!(name(&mut agent, "trailer"), "flatbed", "editor.redo reapplies it");
    // The panel's own undo route for a trailer is the shared one too.
    agent.call("editor.vehicle_tuning_undo", json!({"asset": "trailer"}));
    assert_ne!(name(&mut agent, "trailer"), "flatbed");
    assert_eq!(agent.depth(), base);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_inactive_dirty_table_is_autosaved() {
    let dir = project("undo-global-retained-table");
    let curves = r#"(format_version:1, interp:Linear, curves:{"falloff":(keys:[(0.0,1.0)])})"#;
    std::fs::write(dir.join("assets/a.curves.ron"), curves).expect("a");
    std::fs::write(dir.join("assets/b.curves.ron"), curves).expect("b");
    let mut agent = Agent::new(&dir);
    agent.call("editor.tables_panel_open", json!({"path": "a.curves.ron"}));
    agent.call(
        "editor.tables_panel_curve_edit",
        json!({"op": "set_key", "name": "falloff", "x": 2.0, "y": 0.5}),
    );
    // Opening b keeps a, dirty, behind it.
    agent.call("editor.tables_panel_open", json!({"path": "b.curves.ron"}));
    let saved = agent.call("editor.autosave_now", json!({}));
    let a =
        regia_editor::doc_autosave::DocKey::new("table", Some(std::path::Path::new("assets/a.curves.ron")));
    let b =
        regia_editor::doc_autosave::DocKey::new("table", Some(std::path::Path::new("assets/b.curves.ron")));
    let ids: Vec<&str> = saved["docs"]
        .as_array()
        .expect("docs")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        ids.contains(&a.id().as_str()),
        "the retained dirty table is autosaved: {saved}"
    );
    assert!(
        !ids.contains(&b.id().as_str()),
        "the clean open table is not: {saved}"
    );
    let written = std::fs::read_to_string(
        regia_editor::doc_autosave::docs_dir(&dir).join(format!("{}.autosave", a.id())),
    )
    .expect("the retained table's autosave file");
    assert!(
        written.contains("2.0") || written.contains("2,"),
        "it holds the edit: {written}"
    );
    // Reopening a from disk makes the open copy the one that counts: it is clean, so the
    // retained edit is no longer offered (a save or a reopen decides, as for the open table).
    agent.call("editor.tables_panel_open", json!({"path": "a.curves.ron"}));
    let saved = agent.call("editor.autosave_now", json!({}));
    let ids: Vec<&str> = saved["docs"]
        .as_array()
        .expect("docs")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        !ids.contains(&a.id().as_str()),
        "the reopened clean table is not autosaved: {saved}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_inactive_dirty_vehicle_is_autosaved() {
    let dir = project("undo-global-retained-vehicle");
    let mut agent = Agent::new(&dir);
    agent.call(
        "editor.vehicle_tuning_open",
        json!({"path": "assets/one.vehicle.ron", "create": true}),
    );
    agent.call("editor.vehicle_tuning_save", json!({}));
    agent.call(
        "editor.vehicle_tuning_set",
        json!({"field": "chassis_mass", "value": 1777.0}),
    );
    agent.call(
        "editor.vehicle_tuning_open",
        json!({"path": "assets/two.vehicle.ron", "create": true}),
    );
    agent.call("editor.vehicle_tuning_save", json!({}));
    let saved = agent.call("editor.autosave_now", json!({}));
    let root = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
    let ids: Vec<String> = saved["docs"]
        .as_array()
        .expect("docs")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let index = regia_editor::doc_autosave::read_index(&dir);
    let labels: Vec<&str> = index
        .docs
        .iter()
        .filter(|e| ids.contains(&e.id))
        .map(|e| e.label.as_str())
        .collect();
    assert!(
        labels.contains(&"one.vehicle.ron"),
        "the retained dirty vehicle is autosaved: {saved} {labels:?} (root {})",
        root.display()
    );
    assert!(
        !labels.contains(&"two.vehicle.ron"),
        "the saved open vehicle is not: {labels:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// The error's Regia Engine code.
fn code(err: &RpcError) -> &str {
    err.data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

#[test]
fn a_panels_own_undo_steps_only_its_open_documents_edits() {
    let dir = project("undo-global-own-route");
    let mut agent = Agent::new(&dir);
    let variation = json!({
        "variations": { "Button.own": { "extends": "Button", "props": { "background": "#ca1234" } } }
    });
    // An edit of one draft, then another draft opened and edited (the agent_parity_shell
    // `theme.undo` arm runs this way after earlier theme arms).
    agent.call(
        "editor.theme_panel_open",
        json!({"new": {"name": "own-first", "extends": "regia-dark"}}),
    );
    agent.call("editor.theme_panel_set", variation.clone());
    agent.call(
        "editor.theme_panel_open",
        json!({"new": {"name": "own-second", "extends": "regia-dark"}}),
    );
    agent.call("editor.theme_panel_set", variation.clone());
    let depth = agent.depth();
    let out = agent.call("editor.theme_panel_set", json!({"history": "undo"}));
    assert_eq!(out["changed"], json!(["history"]), "{out}");
    assert_eq!(agent.depth(), depth - 1);
    // The next shared step is the first draft's: the panel's own undo refuses it.
    let err = agent
        .try_call("editor.theme_panel_set", json!({"history": "undo"}))
        .expect_err("one step only for this draft");
    assert_eq!(code(&err), "theme_panel_empty_history", "{err:?}");
    assert_eq!(agent.depth(), depth - 1, "nothing else was undone");
    let out = agent.call("editor.theme_panel_set", json!({"history": "redo"}));
    assert_eq!(out["changed"], json!(["history"]), "{out}");
    assert_eq!(agent.depth(), depth);
    // A scene edit on top: the designer's own undo does not revert it.
    agent.call("editor.ui_designer_open", json!({"new": true}));
    agent.call(
        "editor.ui_designer_edit",
        json!({"op": "add", "preset": "button", "parent": "/"}),
    );
    agent.call("editor.spawn", json!({"name": "on top"}));
    let depth = agent.depth();
    let err = agent
        .try_call("editor.ui_designer_edit", json!({"op": "undo"}))
        .expect_err("the next step is a scene edit");
    assert_eq!(code(&err), "ui_designer_nothing_to_undo", "{err:?}");
    assert_eq!(agent.depth(), depth);
    // The editor's own undo steps everything.
    agent.call("editor.undo", json!({}));
    agent.call("editor.ui_designer_edit", json!({"op": "undo"}));
    assert_eq!(agent.depth(), depth - 2);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_panel_undo_route_is_refused_inside_a_transaction() {
    let dir = project("undo-global-transaction");
    let mut agent = Agent::new(&dir);
    agent.call("editor.ui_anim_open", json!({"new": true}));
    agent.call(
        "editor.ui_anim_edit",
        json!({"op": "add_animation", "name": "fade_in"}),
    );
    agent.call("editor.transaction_begin", json!({"label": "group"}));
    let err = agent
        .try_call("editor.ui_anim_edit", json!({"op": "undo"}))
        .expect_err("refused while edits are grouped");
    assert!(format!("{err:?}").contains("transaction_active"), "{err:?}");
    agent.call(
        "editor.ui_anim_edit",
        json!({"op": "add_animation", "name": "fade_out"}),
    );
    agent.call("editor.transaction_commit", json!({}));
    assert_eq!(animations(&agent), 2);
    agent.call("editor.undo", json!({}));
    assert_eq!(animations(&agent), 1, "the grouped edit undoes as one step");
    let _ = std::fs::remove_dir_all(dir);
}
