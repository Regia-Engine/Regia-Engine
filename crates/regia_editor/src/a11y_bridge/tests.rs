// SPDX-License-Identifier: MPL-2.0
//! The editor window's screen-reader bridge, headless (ADR-7746): the shell's frame through the
//! bridge, the names a reader hears in the toolbars and the Outliner, and reader requests acting on
//! Inspector and Content widgets. No OS reader: tests read the bridge's platform tree (the tree as
//! the OS adapter has it) and push action requests into it the way the adapter's handler does.
// The bridge exists only with the editor's `accesskit` feature (on by default).
#![cfg(feature = "accesskit")]

use egui::accesskit::{Action, ActionData, ActionRequest, NodeId, Role, TreeId, TreeUpdate};
use regia_app::prelude::App;
use serde_json::{Value, json};

use super::*;
use crate::command::EditorCommand as _;
use regia_ecs::prelude::*;

/// A component with a bool and a string field: the Inspector draws a checkbox and a text field.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct Beacon {
    on: bool,
    label: String,
}

/// A project with a level and one asset folder, an edit app with a selected `Rock` that has a
/// `Beacon`, and the shell over it.
struct Harness {
    app: App,
    shell: crate::shell::EditorShell,
    ctx: egui::Context,
    rock: Entity,
    dir: std::path::PathBuf,
    frame: u32,
}

impl Harness {
    fn new(label: &str) -> Self {
        use regia_core::id::Guid;
        use regia_scene::prelude::Transform;
        let dir = regia_test_utils::unique_temp_dir(&format!("a11y-bridge-{label}"));
        std::fs::create_dir_all(dir.join("assets/levels")).expect("dirs");
        std::fs::create_dir_all(dir.join("assets/props")).expect("dirs");
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"A11y\"\nid = \"com.example.a11y\"\n",
        )
        .expect("manifest");
        std::fs::write(
            dir.join("assets/levels/main.level.ron"),
            "( format_version: 1, entities: [] )",
        )
        .expect("level");
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.register_type::<Beacon>();
        let open = crate::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        let mut command = crate::command::SpawnEntities::new_entity(
            app.world(),
            Guid::new_random(),
            "Rock",
            Transform::default(),
        )
        .expect("spawn");
        command.apply(app.world_mut());
        app.world_mut()
            .get_resource_or_insert_with::<crate::command::UndoStack>(crate::command::UndoStack::new)
            .push(Box::new(command));
        let rock = {
            let world = app.world_mut();
            let mut query = world.query::<(Entity, &regia_scene::prelude::Name)>();
            query
                .iter(world)
                .find(|(_, name)| name.as_str() == "Rock")
                .map(|(entity, _)| entity)
                .expect("rock")
        };
        app.world_mut().entity_mut(rock).insert(Beacon::default());
        app.world_mut()
            .get_resource_or_insert_with(crate::selection::Selection::default)
            .select(rock, crate::selection::SelectMode::Replace);
        // What `editor.a11y_keep_tree { on: true }` does: the tree is built without a reader.
        app.world_mut()
            .get_resource_or_insert_with(EditorA11y::default)
            .keep_tree_on(true);
        let shell = crate::shell::EditorShell::new(None, Some(dir.clone()));
        Self {
            app,
            shell,
            ctx: egui::Context::default(),
            rock,
            dir,
            frame: 0,
        }
    }

    /// One editor frame as the run loop draws it: `before_frame`, the shell, `after_frame`.
    fn frame(&mut self) {
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1600.0, 1000.0),
            )),
            time: Some(f64::from(self.frame) / 60.0),
            focused: true,
            ..Default::default()
        };
        self.frame += 1;
        let world = self.app.world_mut();
        before_frame(&self.ctx, world, &mut raw);
        let shell = &mut self.shell;
        let mut output = self.ctx.run_ui(raw, |ui| shell.ui(ui, world, None));
        after_frame(&self.ctx, world, &mut output.platform_output);
        output.textures_delta.clear();
        assert!(
            output.platform_output.accesskit_update.is_none(),
            "after_frame takes the tree out of egui's output"
        );
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            self.frame();
        }
    }

    fn bridge(&self) -> regia_platform::accessibility::AccessKitBridge {
        self.app.world().resource::<EditorA11y>().bridge().clone()
    }

    /// The tree the OS adapter would have now (the bridge's platform tree).
    fn tree(&self) -> TreeUpdate {
        self.bridge()
            .platform_tree()
            .full_update()
            .expect("the bridge holds a whole tree")
    }

    /// The pane or toolbar node `name` of `role`.
    fn region(&self, tree: &TreeUpdate, role: Role, name: &str) -> NodeId {
        find_region(tree, role, name).unwrap_or_else(|| {
            let regions: Vec<String> = tree
                .nodes
                .iter()
                .filter(|(_, n)| matches!(n.role(), Role::Pane | Role::Toolbar))
                .map(|(_, n)| format!("{:?} {:?}", n.role(), n.label()))
                .collect();
            panic!("no {role:?} {name:?} in the tree; regions: {regions:?}")
        })
    }

    /// The one node under `region` with `role` and `name`.
    fn widget(&self, tree: &TreeUpdate, region: NodeId, role: Role, name: &str) -> NodeId {
        let hits: Vec<NodeId> = subtree(tree, region)
            .into_iter()
            .filter(|(_, n)| n.role() == role && n.label() == Some(name))
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "one {role:?} {name:?}; widgets: {:?}",
            widgets_in(tree, region)
        );
        hits[0]
    }

    /// What the OS adapter's handler does when the reader asks `node` to act.
    fn act(&self, node: NodeId, action: Action, data: Option<ActionData>) {
        self.bridge().push_action(ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node: node,
            data,
        });
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn roles_in(tree: &TreeUpdate, region: NodeId) -> Vec<String> {
    widgets_in(tree, region)
        .into_iter()
        .map(|(role, _)| role)
        .collect()
}

/// Item 2 (FU-1): the tree the bridge would send names a button, a text field and a checkbox in
/// the Inspector and the Content browser, and a reader's default action (click) on the
/// Inspector's buttons runs their ops: it opens the `Beacon` section, ticks its checkbox and
/// removes the component.
#[test]
fn a11y_bridge_reader_operates_the_inspector_and_content_browser() {
    let mut h = Harness::new("operate");
    h.frames(3);
    let tree = h.tree();
    let root = tree.tree.as_ref().map(|t| t.root).expect("root");
    assert_eq!(node(&tree, root).and_then(|n| n.label()), Some(WINDOW_NAME));

    // The Content browser: its filter field, a folder button, its checkboxes.
    let content = h.region(&tree, Role::Pane, "Content");
    let content_widgets = widgets_in(&tree, content);
    for role in ["Button", "TextInput"] {
        assert!(
            content_widgets.iter().any(|(r, name)| r == role && spoken(name)),
            "a named {role} in the Content browser: {content_widgets:?}"
        );
    }

    // The Inspector: open the collapsed Beacon and Name sections with a reader's clicks.
    let inspector = h.region(&tree, Role::Pane, "Inspector");
    let header = h.widget(&tree, inspector, Role::Button, "Beacon");
    let name_header = h.widget(&tree, inspector, Role::Button, "Name");
    h.act(header, Action::Click, None);
    h.act(name_header, Action::Click, None);
    h.frames(3);
    let tree = h.tree();
    let inspector = h.region(&tree, Role::Pane, "Inspector");
    let content = h.region(&tree, Role::Pane, "Content");
    let roles = roles_in(&tree, inspector);
    for role in ["Button", "CheckBox"] {
        assert!(
            roles.iter().any(|r| r == role),
            "a {role} in the Inspector: {:?}",
            widgets_in(&tree, inspector)
        );
    }
    // A named text field in each area (review #10): Beacon's `label` in the Inspector (the opaque
    // `Name` draws no text field), the filter in Content.
    for (area, region) in [("Inspector", inspector), ("Content", content)] {
        let widgets = widgets_in(&tree, region);
        assert!(
            widgets
                .iter()
                .any(|(role, name)| role == "TextInput" && spoken(name)),
            "a named TextInput in the {area}: {widgets:?}"
        );
    }
    // FU-11 (docs/evidence/editor-a11y-2026-11.md): the Transform's nine drag values (translation,
    // rotation, scale; x, y, z) carry no name, because the axis labels are not tied to them. Exactly
    // those nine are allowed (review #10): they read the Rock's identity Transform in draw order,
    // and every other Inspector widget, number fields of other sections included, has a name.
    let unnamed_spins: Vec<Option<f64>> = subtree(&tree, inspector)
        .into_iter()
        .filter(|(_, n)| n.role() == Role::SpinButton && !spoken(&spoken_name(&tree, n)))
        .map(|(_, n)| n.numeric_value())
        .collect();
    assert_eq!(
        unnamed_spins,
        [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0].map(Some).to_vec(),
        "only the Transform's nine drag values are unnamed (FU-11)"
    );
    let unspoken: Vec<String> = unspoken_in(&tree, inspector)
        .into_iter()
        .filter(|w| w != "SpinButton \"\"")
        .collect();
    assert_eq!(
        unspoken,
        Vec::<String>::new(),
        "every other Inspector widget has a spoken name"
    );

    // Tick the checkbox.
    let checkbox = h.widget(&tree, inspector, Role::CheckBox, "on");
    h.act(checkbox, Action::Click, None);
    h.frames(2);
    assert_eq!(
        h.app.world().get::<Beacon>(h.rock),
        Some(&Beacon {
            on: true,
            ..Beacon::default()
        }),
        "the reader's click ticked the checkbox"
    );

    // Remove the component: the button's op runs.
    let tree = h.tree();
    let inspector = h.region(&tree, Role::Pane, "Inspector");
    let remove = h.widget(&tree, inspector, Role::Button, "Remove Beacon");
    h.act(remove, Action::Click, None);
    h.frames(2);
    assert!(
        h.app.world().get::<Beacon>(h.rock).is_none(),
        "the reader's click on Remove Beacon removed the component"
    );
    let state = h.app.world().resource::<EditorA11y>();
    assert_eq!(state.actions_injected(), 4);
    assert!(state.frames_sent() >= 10);
    assert_eq!(
        h.bridge().stats().invalid_updates,
        0,
        "every frame's tree passed the adapter's check"
    );
}

/// Item 2: focus and set value from a reader reach egui: a reader focuses the Inspector's first
/// drag value (the Transform's), then sets it to 2.5, and the entity moves.
#[test]
fn a11y_bridge_focus_and_set_value_reach_a_drag_value() {
    let mut h = Harness::new("set-value");
    h.frames(3);
    let tree = h.tree();
    let inspector = h.region(&tree, Role::Pane, "Inspector");
    let spin = subtree(&tree, inspector)
        .into_iter()
        .find(|(_, n)| n.role() == Role::SpinButton)
        .map(|(id, _)| id)
        .unwrap_or_else(|| {
            panic!(
                "a drag value in the Inspector: {:?}",
                widgets_in(&tree, inspector)
            )
        });
    h.act(spin, Action::Focus, None);
    h.frames(2);
    assert_eq!(
        h.bridge().platform_tree().focus,
        Some(spin),
        "the reader's focus request moved egui's focus"
    );
    let before = h
        .app
        .world()
        .get::<regia_scene::prelude::Transform>(h.rock)
        .copied()
        .expect("transform");
    h.act(spin, Action::SetValue, Some(ActionData::NumericValue(2.5)));
    h.frames(3);
    let after = h
        .app
        .world()
        .get::<regia_scene::prelude::Transform>(h.rock)
        .copied()
        .expect("transform");
    assert_ne!(before, after, "the reader's set_value changed the Transform");
    let tree = h.tree();
    assert_eq!(
        node(&tree, spin).and_then(|n| n.numeric_value()),
        Some(2.5),
        "the tree reports the new value"
    );
}

/// Operable widgets of these areas that may keep a name with no letter: none today. Each entry:
/// (area, `Role "name"`, why).
const UNSPOKEN_ALLOWED: &[(&str, &str, &str)] = &[];

/// Item 3 (FU-1): every operable widget in the play toolbar, the Outliner (rows with their eye and
/// lock) and the viewport toolbar has a name a reader can speak.
#[test]
fn a11y_bridge_toolbars_and_outliner_rows_have_spoken_names() {
    let mut h = Harness::new("names");
    h.frames(3);
    let tree = h.tree();
    let areas = [
        (Role::Toolbar, PLAY_TOOLBAR),
        (Role::Pane, "Outliner"),
        (Role::Toolbar, VIEWPORT_TOOLBAR),
    ];
    let mut failures = Vec::new();
    for (role, area) in areas {
        let region = h.region(&tree, role, area);
        let widgets = widgets_in(&tree, region);
        assert!(!widgets.is_empty(), "{area} has operable widgets");
        for unspoken in unspoken_in(&tree, region) {
            if !UNSPOKEN_ALLOWED
                .iter()
                .any(|(a, widget, _)| *a == area && *widget == unspoken)
            {
                failures.push(format!("{area}: {unspoken}"));
            }
        }
    }
    assert_eq!(failures, Vec::<String>::new(), "unnamed operable widgets");

    // The Outliner row's eye and lock speak what they do.
    let outliner = h.region(&tree, Role::Pane, "Outliner");
    let names: Vec<String> = widgets_in(&tree, outliner).into_iter().map(|(_, n)| n).collect();
    for want in [
        "Hide Rock in the viewport",
        "Lock Rock: not pickable in the viewport",
    ] {
        assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
    }
}

/// Calls `method` through the editor's agent extension, as the dispatcher does.
fn call(world: &mut regia_ecs::prelude::World, method: &str, params: Value) -> Result<Value, String> {
    if world
        .get_resource::<regia_agent::extension::AgentExtension>()
        .is_none()
    {
        crate::agent_ext::install(world);
    }
    let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params).map_err(|e| {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("no_code")
            .to_owned()
    })
}

/// A profile that grants `review` and `build` on top of the default one: what `editor.a11y_action`
/// needs (ADR-7795).
fn ui_driver_profile() -> regia_agent::AgentPermissions {
    let mut permissions = regia_agent::AgentPermissions::default();
    for name in ["review", "build"] {
        permissions.config.permissions.insert(name.to_owned());
    }
    permissions
}

/// The agent surface end to end: `editor.a11y_keep_tree { on: true }` turns the tree on, the next
/// frame has it, and `editor.a11y_action` (with review and build granted) clicks a named button
/// through the same path as a reader.
#[test]
fn a11y_bridge_agent_reads_the_tree_and_clicks_by_name() {
    let mut h = Harness::new("agent");
    h.app.world_mut().resource_mut::<EditorA11y>().keep_tree_on(false);
    h.app.world_mut().insert_resource(ui_driver_profile());
    h.frame();
    let world = h.app.world_mut();
    assert_eq!(
        call(
            world,
            "editor.a11y_action",
            json!({ "action": "click", "name": "Beacon" })
        ),
        Err("no_tree".to_owned()),
        "no tree before anyone asked for one"
    );
    assert_eq!(
        call(world, "editor.a11y_tree", json!({ "keep_on": true })),
        Err("keep_on_moved".to_owned()),
        "the read-only tree call changes nothing"
    );
    assert_eq!(
        call(world, "editor.a11y_keep_tree", json!({})),
        Err("invalid_value".to_owned())
    );
    let kept = call(world, "editor.a11y_keep_tree", json!({ "on": true })).expect("keep");
    assert_eq!(kept, json!({ "keep_on": true, "tree_on": false }));
    let first = call(world, "editor.a11y_tree", json!({})).expect("tree");
    assert_eq!(first["keep_on"], true);
    assert_eq!(first["nodes"], json!([]), "the tree arrives with the next frame");
    h.frames(2);
    let world = h.app.world_mut();
    let panes = call(
        world,
        "editor.a11y_tree",
        json!({ "role": "Pane", "name": "Inspector" }),
    )
    .expect("tree");
    assert_eq!(panes["tree_on"], true);
    assert_eq!(panes["nodes"].as_array().map(Vec::len), Some(1), "{panes}");
    let buttons = call(
        world,
        "editor.a11y_tree",
        json!({ "role": "Button", "name": "Beacon" }),
    )
    .expect("tree");
    assert_eq!(
        buttons["nodes"][0]["actions"],
        json!(["click", "focus"]),
        "{buttons}"
    );
    assert_eq!(
        call(
            world,
            "editor.a11y_action",
            json!({ "action": "press", "name": "Beacon" })
        ),
        Err("unknown_action".to_owned())
    );
    assert_eq!(
        call(
            world,
            "editor.a11y_action",
            json!({ "action": "click", "name": "No such widget" })
        ),
        Err("node_not_found".to_owned())
    );
    assert_eq!(
        call(
            world,
            "editor.a11y_action",
            json!({ "action": "click", "node": 1u64 << 60 })
        ),
        Err("node_not_found".to_owned())
    );
    assert_eq!(
        call(world, "editor.a11y_action", json!({ "action": "click" })),
        Err("missing_target".to_owned())
    );
    assert_eq!(
        call(
            world,
            "editor.a11y_action",
            json!({ "action": "set_value", "name": "Beacon", "value": [1] })
        ),
        Err("invalid_value".to_owned())
    );
    let queued = call(
        world,
        "editor.a11y_action",
        json!({ "action": "click", "name": "Beacon", "role": "Button" }),
    )
    .expect("queued");
    assert_eq!(queued["queued"], true);
    h.frames(3);
    let world = h.app.world_mut();
    let on = call(
        world,
        "editor.a11y_tree",
        json!({ "role": "CheckBox", "name": "on" }),
    )
    .expect("tree");
    assert_eq!(
        on["nodes"].as_array().map(Vec::len),
        Some(1),
        "the click opened Beacon: {on}"
    );
    let removes = call(world, "editor.a11y_tree", json!({ "name": "Remove Beacon" })).expect("tree");
    assert_eq!(removes["nodes"].as_array().map(Vec::len), Some(1), "{removes}");
    let _ = call(world, "editor.a11y_keep_tree", json!({ "on": false })).expect("off");
    h.frame();
    assert!(
        !h.app.world().resource::<EditorA11y>().tree_on(),
        "on: false turns the tree off"
    );
}

/// An editor app with the agent dispatcher and the editor's methods, answering requests from an
/// in-process inbox within one frame (no project: the default profile, as `regia editor` without
/// `.regia/agent.toml`).
struct Dispatcher {
    app: App,
    inbox: crossbeam_channel::Sender<regia_agent::server::Inbound>,
    next_id: u64,
}

impl Dispatcher {
    fn new() -> Self {
        let mut app = App::new();
        app.add_plugins(regia_app::prelude::MinimalPlugins);
        app.add_plugins(regia_agent::AgentPlugin);
        crate::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        Self {
            app,
            inbox,
            next_id: 0,
        }
    }

    /// One call through the dispatcher (scope, permission, contract, rate limit, handler): the
    /// result, or the error's `data.code`.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        let request = regia_agent::protocol::Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(self.next_id)),
            method: method.into(),
            params,
        };
        self.inbox
            .send(regia_agent::server::Inbound { request, reply })
            .expect("inbox open");
        self.app.update();
        let response = answer.try_recv().expect("answered within the frame");
        match (response.result, response.error) {
            (_, Some(error)) => Err(error
                .data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("no_code")
                .to_owned()),
            (Some(value), None) => Ok(value),
            (None, None) => Err("empty_response".to_owned()),
        }
    }
}

/// Review #1 (ADR-7795): `editor.a11y_action` can press any editor control, the Agent Changes
/// panel's Accept (review) and the package, launch and plugin controls (build) included, so an
/// agent on the default profile (read, edit_scene, assets, play) is refused before a target is
/// even looked up, and so is one granted `review` alone. Only `review` plus `build` reaches the
/// handler (which then reports that no tree was sent yet).
#[test]
fn a11y_bridge_default_profile_agent_cannot_press_accept() {
    let mut d = Dispatcher::new();
    let press_accept = json!({ "action": "click", "name": "Accept", "role": "Button" });
    assert_eq!(
        d.call("editor.a11y_action", press_accept.clone()),
        Err("F0012".to_owned()),
        "a default-profile agent cannot press Accept on its own proposal"
    );
    let mut review_only = regia_agent::AgentPermissions::default();
    review_only.config.permissions.insert("review".to_owned());
    d.app.world_mut().insert_resource(review_only);
    assert_eq!(
        d.call("editor.a11y_action", press_accept.clone()),
        Err("F0012".to_owned()),
        "review alone does not open the package, launch and plugin controls"
    );
    d.app.world_mut().insert_resource(ui_driver_profile());
    assert_eq!(
        d.call("editor.a11y_action", press_accept),
        Err("no_tree".to_owned()),
        "review and build reach the handler"
    );
}

/// Review #11 (ADR-7795): `editor.a11y_tree` is read only and changes nothing: a read-only client
/// cannot make egui build its tree every frame. The switch is `editor.a11y_keep_tree`, a mutating
/// call that the read-only profile refuses and the default profile allows.
#[test]
fn a11y_bridge_read_only_client_cannot_turn_the_tree_on() {
    let mut d = Dispatcher::new();
    d.app.world_mut().insert_resource(regia_agent::AgentPermissions {
        config: regia_agent::AgentConfig::fail_closed(),
        source: None,
    });
    let _ = d.call("editor.a11y_tree", json!({ "keep_on": true }));
    let listing = d
        .call("editor.a11y_tree", json!({}))
        .expect("reading stays allowed");
    assert_eq!(
        listing["keep_on"], false,
        "a read-only call left the tree switched on"
    );
    assert_eq!(
        d.call("editor.a11y_keep_tree", json!({ "on": true })),
        Err("F0012".to_owned()),
        "the read-only profile cannot turn the tree on"
    );
    d.app
        .world_mut()
        .insert_resource(regia_agent::AgentPermissions::default());
    assert_eq!(
        d.call("editor.a11y_keep_tree", json!({ "on": true })),
        Ok(json!({ "keep_on": true, "tree_on": false }))
    );
    let listing = d.call("editor.a11y_tree", json!({})).expect("tree");
    assert_eq!(
        listing["keep_on"], true,
        "the default profile's edit_scene turns it on"
    );
}

/// Review #4 (ADR-7795): the editor's default `accesskit` feature brings in the OS adapter types
/// only (`regia_platform/accesskit-adapters`), never the runtime bridge's `Last` system. A
/// workspace build unifies features across crates, so that system would otherwise join every
/// `PlatformPlugin` app the gate's suites build (schedule pins, ambiguity reports, allocation
/// counts). This test runs with the editor's features: it fails if they turn on the runtime one.
#[test]
fn a11y_bridge_feature_adds_no_system_to_platform_apps() {
    let mut app = App::new();
    app.add_plugins(regia_platform::PlatformPlugin);
    let schedules = app.world().resource::<regia_ecs::ecs::schedule::Schedules>();
    let accessibility_systems: Vec<String> = schedules
        .iter()
        .flat_map(|(label, schedule)| {
            schedule
                .graph()
                .systems
                .iter()
                .map(move |(_, system, _)| format!("{label:?}: {}", system.name()))
        })
        .filter(|name| name.contains("accessibility"))
        .collect();
    assert_eq!(
        accessibility_systems,
        Vec::<String>::new(),
        "PlatformPlugin systems from the editor's accesskit feature"
    );
}

/// A glyph alone is not a spoken name; a name with a letter or digit is.
#[test]
fn a11y_bridge_spoken_names_need_a_letter_or_digit() {
    for glyph in ["\u{1f441}", "\u{1f512}", "\u{2013}", "\u{25b8}", "", " "] {
        assert!(!spoken(glyph), "{glyph:?}");
    }
    for name in ["\u{25b6} Play", "Hide in the viewport", "4 views"] {
        assert!(spoken(name), "{name:?}");
    }
}

/// OS tree pass (ADR-8587, `docs/evidence/editor-a11y-os-trees-2026-11.md`): the three OS adapters name a
/// `Label` node by its value (its text) and ignore a label set on it, as egui itself fills a label's
/// node. `name_node` on a draggable label (the viewport overlays' `::` grips) must therefore set the
/// text a reader speaks; before, UI Automation read the grips as "::".
#[test]
fn a11y_bridge_name_node_on_a_label_sets_the_text_a_reader_speaks() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut grip = None;
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        let response = ui.add(egui::Label::new("::").sense(egui::Sense::drag()));
        name_node(&response, "Drag Camera overlay");
        grip = Some(response.id);
    });
    // Unapplied texture deltas panic when dropped (and a failed assert would then abort).
    output.textures_delta.clear();
    let update = output.platform_output.accesskit_update.expect("accesskit is on");
    let id = grip.expect("grip drawn").accesskit_id();
    let grip = node(&update, id).expect("the grip's node");
    assert_eq!(grip.role(), Role::Label);
    assert_eq!(
        grip.value(),
        Some("Drag Camera overlay"),
        "a Label speaks its value: the name must land there"
    );
    assert_eq!(spoken_name(&update, grip), "Drag Camera overlay");
}

/// OS tree pass (ADR-8587): `spoken_name` follows the adapters' rule: a `Label` is named by its value
/// even when it also has a label; another role by its label, else by the nodes it is labelled by
/// (a `Label` among them contributing its value).
#[test]
fn a11y_bridge_spoken_name_follows_the_os_adapters() {
    let mut label = Node::new(Role::Label);
    label.set_label("ignored by every adapter");
    label.set_value("::");
    let mut caption = Node::new(Role::Label);
    caption.set_label("not this");
    caption.set_value("Preview language");
    let mut combo = Node::new(Role::ComboBox);
    combo.set_labelled_by(vec![NodeId(3)]);
    combo.set_value("Source");
    let mut root = Node::new(Role::Window);
    root.set_children(vec![NodeId(2), NodeId(3), NodeId(4)]);
    let update = TreeUpdate {
        nodes: vec![
            (NodeId(1), root),
            (NodeId(2), label),
            (NodeId(3), caption),
            (NodeId(4), combo),
        ],
        tree: Some(egui::accesskit::Tree::new(NodeId(1))),
        tree_id: TreeId::ROOT,
        focus: NodeId(1),
    };
    let name = |id: u64| spoken_name(&update, node(&update, NodeId(id)).expect("node"));
    assert_eq!(name(2), "::", "a Label speaks its value");
    assert_eq!(
        name(4),
        "Preview language",
        "labelled by a Label: that Label's value"
    );
}

/// OS tree pass (ADR-8587): `editor.a11y_tree` answers each node's name as the OS adapter exposes it,
/// so an agent sees what a reader hears and can address it: the play toolbar's language picker has
/// no label of its own and is named "Preview language" by the label beside it (UI Automation read
/// that name on the Windows walk while the agent answered ""), and a `Label` is named by its text.
#[test]
fn a11y_bridge_agent_names_nodes_as_the_os_does() {
    let mut h = Harness::new("os-names");
    h.app.world_mut().insert_resource(ui_driver_profile());
    h.frames(3);
    let world = h.app.world_mut();
    let picker = call(
        world,
        "editor.a11y_tree",
        json!({ "role": "ComboBox", "name": "Preview language" }),
    )
    .expect("tree");
    assert_eq!(
        picker["nodes"].as_array().map(Vec::len),
        Some(1),
        "the language picker by its spoken name: {picker}"
    );
    let entity = call(
        world,
        "editor.a11y_tree",
        json!({ "role": "Label", "name": "Entity: Rock" }),
    )
    .expect("tree");
    assert_eq!(
        entity["nodes"].as_array().map(Vec::len),
        Some(1),
        "a Label is named by its text: {entity}"
    );
    let queued = call(
        world,
        "editor.a11y_action",
        json!({ "action": "focus", "name": "Preview language", "role": "ComboBox" }),
    )
    .expect("an agent addresses the node by the name it read");
    assert_eq!(queued["queued"], true);
}
