// SPDX-License-Identifier: MPL-2.0
//! Every shell action of the action registry (`regia_editor::agent_parity::ACTIONS`, panels
//! `shell.*`: the File, Create, Edit and View menus, the Play toolbar, the prefab banner and the
//! unsaved-changes prompt) driven headless through its agent method or `editor.command` id, over
//! the real dispatcher, with its effect checked where it is observable (ADR-7307).
//!
//! A new shell action without a case here fails `every_shell_action_runs_headless`.

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::agent_parity::{ACTIONS, ActionPath};
use regia_editor::command::UndoStack;
use regia_editor::editor_layout::{LayoutRequest, LayoutRequests};
use regia_editor::level_io::{self, OpenLevel};
use regia_editor::pie::{PieRequest, PieRequests};
use regia_editor::selection::Selection;
use regia_editor::shell::Panel;
use regia_editor::viewport::{EditorCamera, ViewportSettings};
use serde_json::{Value, json};

struct Agent {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
}

impl Agent {
    /// The editor binary's edit world with the agent plugin; the socket inbox is swapped for an
    /// in-process one (as `undo_agent_smoke` does).
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
            .init_resource::<EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<ViewportSettings>();
        let open = level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        app.world_mut()
            .insert_resource(regia_editor::agent_ext::EditorProject {
                root: Some(project.to_path_buf()),
            });
        regia_editor::agent_ext::install(app.world_mut());
        regia_editor::headless::install(app.world_mut());
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
        self.try_call(method, params.clone())
            .unwrap_or_else(|err| panic!("{method} {params}: {err:?}"))
    }

    fn library_call(&mut self, params: Value) -> Value {
        let reply = self.call("editor.asset_library", params);
        if let Some(job) = reply["job"].as_str() {
            regia_editor::content_library::finish_copies(self.world());
            let status = self.call("agent.job_status", json!({"job": job}));
            assert_eq!(status["state"], "done", "{status}");
            status["result"].clone()
        } else {
            reply
        }
    }

    /// `editor.command { run }` and its status line.
    fn command(&mut self, id: &str) -> String {
        let out = self.call("editor.command", json!({ "run": id, "limit": 1 }));
        assert_eq!(out["ran"]["id"], id, "{out}");
        out["ran"]["message"].as_str().unwrap_or_default().to_owned()
    }

    fn world(&mut self) -> &mut World {
        self.app.world_mut()
    }

    fn names(&mut self) -> Vec<String> {
        let world = self.world();
        let mut query = world.query::<&regia_scene::prelude::Name>();
        query.iter(world).map(|n| n.as_str().to_owned()).collect()
    }

    fn layout_requests(&mut self) -> Vec<LayoutRequest> {
        self.world()
            .get_resource::<LayoutRequests>()
            .map(|r| r.pending.clone())
            .unwrap_or_default()
    }

    fn undo_depth(&mut self) -> usize {
        self.world().resource::<UndoStack>().undo_depth()
    }
}

fn project() -> std::path::PathBuf {
    let dir = regia_test_utils::unique_temp_dir("agent-parity-shell");
    std::fs::create_dir_all(dir.join("assets/maps")).expect("assets dir");
    dir
}

/// The `data.reason` of an editor error.
fn reason(err: &RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The menu tree's item for a registered action (what `editor.menu_list` answers for it).
fn menu_item(id: &str) -> regia_editor::menus::MenuItem {
    regia_editor::menus::menu_tree()
        .into_iter()
        .find(|item| item.action.id == id)
        .unwrap_or_else(|| panic!("{id} is in the menu tree"))
}

/// Grants the `build` permission (packaging and Clear caches; not in the default profile).
fn grant_build(agent: &mut Agent) {
    agent
        .world()
        .resource_mut::<regia_agent::AgentPermissions>()
        .config
        .permissions
        .insert("build".into());
}

fn code(err: &RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// A 9 × 9 sample, 8 m heightfield terrain (the `terrain_kit_tools` fixture).
fn small_terrain() -> regia_terrain::Terrain {
    let hf = regia_asset::Heightfield {
        width: 9,
        height: 9,
        samples: (0..81).map(|i| i * 123).collect(),
    };
    regia_terrain::Terrain::new(
        &hf,
        8.0,
        100.0,
        0.0,
        regia_terrain::CdlodConfig::default(),
        2,
        regia_terrain::TerrainLayers::default(),
    )
    .expect("terrain")
}

/// Lifts the heavy-method rate limit (terrain regions, kit painting) for back-to-back calls.
fn allow_heavy(agent: &mut Agent) {
    agent
        .world()
        .resource_mut::<regia_agent::AgentPermissions>()
        .config
        .heavy_rate_limit_per_second = 10_000;
}

#[test]
fn every_shell_action_runs_headless() {
    let dir = project();
    let mut agent = Agent::new(&dir);
    let shell: Vec<_> = ACTIONS
        .iter()
        .filter(|a| a.panel.starts_with("shell") || a.panel.starts_with("viewport"))
        .collect();
    assert!(
        shell.len() >= 30,
        "the shell registers its menus: {}",
        shell.len()
    );
    for action in shell {
        match action.id {
            "file.create_entity" => {
                assert_eq!(action.path, ActionPath::Method("editor.spawn"));
                let before = agent.undo_depth();
                let out = agent.call("editor.spawn", json!({}));
                assert!(out["entity"].as_u64().is_some(), "{out}");
                assert!(agent.names().iter().any(|n| n == "New entity"));
                assert_eq!(agent.undo_depth(), before + 1);
            }
            "create.shape" => {
                for kind in regia_render::PrimitiveShape::KINDS {
                    let message = agent.command(&format!("create.{kind}"));
                    let label = regia_editor::shapes::shape_label(kind);
                    assert_eq!(message, format!("{label} created"));
                    assert!(agent.names().contains(&label), "{kind}");
                }
            }
            "edit.undo" => {
                let before = agent.undo_depth();
                assert!(before > 0);
                assert_eq!(agent.command("edit.undo"), "Undone");
                assert_eq!(agent.undo_depth(), before - 1);
            }
            "edit.redo" => {
                let before = agent.undo_depth();
                assert_eq!(agent.command("edit.redo"), "Redone");
                assert_eq!(agent.undo_depth(), before + 1);
            }
            "file.save_level" | "file.save_level_as" => {
                let name = format!("assets/maps/{}.level.ron", action.id.replace('.', "_"));
                let out = agent.call("editor.save_level", json!({ "path": name }));
                assert!(dir.join(&name).is_file(), "{out}");
                assert!(!agent.world().resource::<UndoStack>().is_dirty());
            }
            "file.open_level" => {
                let path = "assets/maps/open_me.level.ron";
                agent.call("editor.save_level", json!({ "path": path }));
                let out = agent.call("editor.open_level", json!({ "path": path, "discard": true }));
                assert!(
                    out["path"]
                        .as_str()
                        .is_some_and(|p| p.ends_with("open_me.level.ron")),
                    "{out}"
                );
            }
            "file.new_level" => {
                agent.call("editor.spawn", json!({ "name": "Unsaved" }));
                let level = agent.world().resource::<OpenLevel>().id;
                let refused = agent.command("file.new_level");
                assert!(refused.starts_with("Unsaved changes"), "{refused}");
                assert_eq!(
                    agent.world().resource::<OpenLevel>().id,
                    level,
                    "refused: same level"
                );
                assert_eq!(agent.command("file.new_level_discard"), "New level");
                assert_ne!(agent.world().resource::<OpenLevel>().id, level, "a new level");
                assert_eq!(agent.undo_depth(), 0, "edit state reset");
                assert!(!agent.names().iter().any(|n| n == "Unsaved"));
            }
            "file.open_kit" => {
                let err = agent
                    .try_call("editor.open_kit", json!({ "path": "no-such-kit" }))
                    .expect_err("a missing kit is refused");
                assert!(!code(&err).is_empty(), "{err:?}");
            }
            "file.export_glb" => {
                agent.call("editor.spawn", json!({ "name": "Exported" }));
                agent.call(
                    "editor.save_level",
                    json!({ "path": "assets/maps/export.level.ron" }),
                );
                let out = agent.call(
                    "asset.export_glb",
                    json!({ "level": "assets/maps/export.level.ron", "out": "export/shell.glb" }),
                );
                assert_eq!(out["ok"], true, "{out}");
                let bytes = std::fs::read(dir.join("export/shell.glb")).expect("glb written");
                assert_eq!(&bytes[0..4], b"glTF");
            }
            // Window ▸ New Outliner / Content Browser / Inspector (P-003, ADR-8145): the method
            // queues the lowest free instance; the palette command is the keyboard path.
            "window.new_outliner" | "window.new_content" | "window.new_inspector" => {
                assert_eq!(action.path, ActionPath::Method("editor.panel_new_instance"));
                let (panel, kind) = match action.id {
                    "window.new_outliner" => ("outliner", regia_editor::shell::InstancedPanel::Outliner),
                    "window.new_content" => ("content", regia_editor::shell::InstancedPanel::Content),
                    _ => ("inspector", regia_editor::shell::InstancedPanel::Inspector),
                };
                let out = agent.call("editor.panel_new_instance", json!({ "panel": panel }));
                assert_eq!(out["panel"], format!("{panel}#1"), "{out}");
                assert!(
                    agent
                        .layout_requests()
                        .contains(&LayoutRequest::NewInstance(Panel::Instance(kind, 1))),
                    "{}",
                    action.id
                );
                // The palette command queues the next one.
                agent.command(action.id);
                assert!(
                    agent
                        .layout_requests()
                        .contains(&LayoutRequest::NewInstance(Panel::Instance(kind, 2))),
                    "{}",
                    action.id
                );
            }
            // Window ▸ Utility Panels (ADR-8189): the starter sample is listed and opens as a tab.
            "window.utility_panel" => {
                assert_eq!(action.path, ActionPath::Method("editor.utility_panels"));
                let listed = agent.call("editor.utility_panels", json!({ "op": "list" }));
                let id = regia_editor::utility_panel::STARTER_ID;
                assert_eq!(listed["panels"][0]["id"], id, "{listed}");
                let opened = agent.call("editor.utility_panels", json!({ "op": "open", "id": id }));
                assert_eq!(opened["id"], id, "{opened}");
                assert!(
                    agent.layout_requests().iter().any(
                        |r| matches!(r, LayoutRequest::Open(Panel::UtilityPanel(open, _)) if *open == id)
                    ),
                    "{opened}"
                );
            }
            "window.agent" => {
                assert_eq!(action.path, ActionPath::Command("window.open_agent"));
                agent.command("window.open_agent");
                assert!(
                    agent
                        .layout_requests()
                        .contains(&LayoutRequest::Open(Panel::Agent))
                );
            }
            "edit.preferences" | "edit.project_settings" | "edit.world_settings" => {
                let ActionPath::Command(id) = action.path else {
                    panic!("{}: a palette command", action.id);
                };
                agent.command(id);
                let want = match action.id {
                    "edit.preferences" => Panel::Preferences,
                    "edit.project_settings" => Panel::ProjectSettings,
                    _ => Panel::WorldSettings,
                };
                assert!(
                    agent.layout_requests().contains(&LayoutRequest::Open(want)),
                    "{}",
                    action.id
                );
            }
            "view.layout_save" | "view.layout_save_default" | "view.layout_reset" => {
                let ActionPath::Command(id) = action.path else {
                    panic!("{}: a palette command", action.id);
                };
                agent.command(id);
                let want = match id {
                    "layout.save" => LayoutRequest::SaveProject,
                    "layout.save_default" => LayoutRequest::SaveUser,
                    _ => LayoutRequest::Reset,
                };
                assert!(agent.layout_requests().contains(&want), "{id}");
            }
            // Maximize, full screen, workspaces, named layouts, panels and feature profiles (ADR-7753).
            "view.maximize_panel" | "view.fullscreen_viewport" => {
                let ActionPath::Command(id) = action.path else {
                    panic!("{}: a palette command", action.id);
                };
                agent.command(id);
                let want = if id == "view.maximize_panel" {
                    regia_editor::workspaces::ViewRequest::ToggleMaximize(None)
                } else {
                    regia_editor::workspaces::ViewRequest::ToggleFullscreen
                };
                assert!(
                    agent
                        .world()
                        .get_resource::<regia_editor::workspaces::ViewRequests>()
                        .is_some_and(|r| r.pending.contains(&want)),
                    "{id}"
                );
            }
            "view.workspace" => {
                assert_eq!(action.path, ActionPath::Method("editor.workspace_set"));
                let out = agent.call("editor.workspace_set", json!({ "id": "animation" }));
                assert_eq!(out["queued"]["workspace"], "animation", "{out}");
                let list = agent.call("editor.workspace_list", json!({}));
                assert!(
                    list["workspaces"].as_array().is_some_and(|w| w.len() >= 7),
                    "{list}"
                );
                agent.call(
                    "editor.workspace_set",
                    json!({"id": "terrain", "mode": "terrain"}),
                );
                let mut dock = regia_editor::editor_layout::default_dock();
                let mut views = regia_editor::workspaces::DockViews::default();
                views.apply_requests(&mut dock, agent.world());
                views.publish(agent.world());
                assert_eq!(agent.call("editor.workspace_list", json!({}))["mode"], "terrain");
                assert!(
                    agent
                        .world()
                        .resource::<regia_editor::terrain_tool::TerrainTool>()
                        .active
                );
            }
            "workspace.join" | "workspace.swap" => {
                use regia_editor::workspaces::{DockGroups, DockViews};
                let mut dock = egui_dock::DockState::new(vec![Panel::Viewport, Panel::Content]);
                dock.main_surface_mut().split_right(
                    egui_dock::NodeIndex::root(),
                    0.6,
                    vec![Panel::Inspector, Panel::Outliner],
                );
                DockGroups::refresh(agent.world(), &dock);
                let before = DockGroups::of(&dock);
                let (method, params) = if action.id == "workspace.join" {
                    (
                        "editor.layout_join",
                        json!({"from": "inspector", "into": "viewport"}),
                    )
                } else {
                    (
                        "editor.layout_swap",
                        json!({"from": "viewport", "with": "inspector"}),
                    )
                };
                assert_eq!(action.path, ActionPath::Method(method));
                agent.call(method, params);
                DockViews::default().apply_requests(&mut dock, agent.world());
                let after = DockGroups::of(&dock);
                if action.id == "workspace.join" {
                    assert_eq!(after.groups.len(), 1);
                    assert_eq!(
                        after.groups.values().next().expect("merged"),
                        &vec![Panel::Viewport, Panel::Content, Panel::Inspector, Panel::Outliner]
                    );
                } else {
                    let keys: Vec<_> = before.groups.keys().collect();
                    assert_eq!(after.groups[keys[0]], before.groups[keys[1]]);
                    assert_eq!(after.groups[keys[1]], before.groups[keys[0]]);
                }
            }
            "workspace.tool" => {
                assert_eq!(action.path, ActionPath::Method("editor.layout_apply"));
                agent.call("editor.layout_apply", json!({"open": "terrain"}));
                assert!(
                    agent
                        .layout_requests()
                        .contains(&LayoutRequest::Open(Panel::Terrain))
                );
            }
            "view.named_layouts" => {
                assert_eq!(action.path, ActionPath::Method("editor.layout_save_named"));
                // This agent's editor keeps no per-user files.
                let err = agent
                    .try_call("editor.layout_save_named", json!({ "name": "Wide" }))
                    .expect_err("no user config");
                assert_eq!(code(&err), "no_user_config");
            }
            "view.panels" => {
                assert_eq!(action.path, ActionPath::Method("editor.layout_apply"));
                agent.call("editor.layout_apply", json!({ "open": "layers" }));
                assert!(
                    agent
                        .layout_requests()
                        .contains(&LayoutRequest::Open(regia_editor::shell::Panel::Layers))
                );
            }
            "view.feature_profile" => {
                assert_eq!(action.path, ActionPath::Method("editor.feature_profile_set"));
                let out = agent.call("editor.feature_profile_set", json!({ "name": "UI only" }));
                assert_eq!(out["active"]["name"], "UI only", "{out}");
                let out = agent.call("editor.feature_profile_set", json!({ "name": "Full" }));
                assert_eq!(out["hidden_panels"], json!([]), "{out}");
            }
            "view.grid" | "view.labels" | "view.realtime" => {
                let ActionPath::Command(id) = action.path else {
                    panic!("{}: a palette command", action.id);
                };
                let flag = |s: &ViewportSettings| match id {
                    "view.toggle_grid" => s.grid,
                    "view.toggle_labels" => s.labels,
                    _ => s.realtime,
                };
                let before = flag(agent.world().resource::<ViewportSettings>());
                agent.command(id);
                assert_ne!(flag(agent.world().resource::<ViewportSettings>()), before, "{id}");
            }
            // View ▸ Frame Debugger (3253): the window toggles; no capture starts.
            "view.frame_debugger" => {
                assert_eq!(action.path, ActionPath::Command("view.toggle_frame_debugger"));
                assert_eq!(
                    agent.command("view.toggle_frame_debugger"),
                    "Frame Debugger shown"
                );
                assert!(
                    agent
                        .world()
                        .resource::<regia_editor::frame_debugger::FrameDebugger>()
                        .open
                );
                let state = agent.call("editor.frame_capture", json!({ "action": "state" }));
                assert_eq!(state["state"], "idle", "{state}");
                assert_eq!(
                    agent.command("view.toggle_frame_debugger"),
                    "Frame Debugger hidden"
                );
            }
            "view.profiler" => {
                assert_eq!(agent.command("view.toggle_profiler"), "Profiler shown");
                assert!(
                    agent
                        .world()
                        .resource::<regia_editor::profiler_panel::ProfilerPanel>()
                        .open
                );
            }
            "view.diagnostics" => {
                assert_eq!(agent.command("view.toggle_diagnostics"), "Diagnostics shown");
                assert!(
                    agent
                        .world()
                        .resource::<regia_editor::diagnostics_panel::DiagnosticsPanel>()
                        .open
                );
            }
            "view.revision_control" => {
                assert_eq!(
                    agent.command("view.toggle_revision_control"),
                    "Revision Control shown"
                );
                assert!(
                    agent
                        .world()
                        .resource::<regia_editor::vcs::panel::VcsPanel>()
                        .open
                );
            }
            "view.frame_selection" => {
                let out = agent.call(
                    "editor.spawn",
                    json!({ "name": "Far", "translation": [40.0, 0.0, 0.0] }),
                );
                let entity = out["entity"].as_u64().expect("entity");
                agent.call("editor.select", json!({ "entities": [entity] }));
                let before = agent.world().resource::<EditorCamera>().target;
                assert_eq!(agent.command("view.frame_selection"), "Framed the selection");
                let after = agent.world().resource::<EditorCamera>().target;
                assert!(
                    after != before && (after.x - 40.0).abs() < 1.0,
                    "{before:?} -> {after:?}"
                );
            }
            "view.reset_zoom" => {
                // ADR-7733: the interface zoom is the saved UI scale; zoom in, then reset to 100 %.
                let scale = |agent: &mut Agent| {
                    let out = agent.call("editor.prefs_get", json!({ "query": "ui_scale" }));
                    out["prefs"]
                        .as_array()
                        .and_then(|p| p.iter().find(|p| p["key"] == "appearance.ui_scale"))
                        .and_then(|p| p["value"].as_f64())
                        .unwrap_or_else(|| panic!("ui_scale: {out}"))
                };
                assert_eq!(agent.command("view.zoom_in"), "Interface zoom 110 %");
                assert_eq!(scale(&mut agent), 1.1);
                assert_eq!(agent.command("view.reset_zoom"), "Interface zoom 100 %");
                assert_eq!(scale(&mut agent), 1.0);
            }
            "play.play" | "play.pause" | "play.resume" | "play.stop" | "play.step" => {
                let ActionPath::Method(method) = action.path else {
                    panic!("{}: a method", action.id);
                };
                agent.world().insert_resource(PieRequests::default());
                let out = agent.call(method, json!({}));
                assert_eq!(out["queued"], true, "{out}");
                let want = match method {
                    "editor.play" => PieRequest::Play,
                    "editor.pause" => PieRequest::Pause,
                    "editor.resume" => PieRequest::Resume,
                    "editor.stop" => PieRequest::Stop,
                    _ => PieRequest::Step(1),
                };
                let queue = agent.world().resource::<PieRequests>().queue.clone();
                assert_eq!(queue.first(), Some(&want), "{method}: {queue:?}");
            }
            "play.compile" => {
                assert_eq!(action.path, ActionPath::Method("agent.hot_reload"));
                // No game crate in this project: the call answers (an error naming why), never hangs.
                let answer = agent.try_call("agent.hot_reload", json!({}));
                if let Err(err) = &answer {
                    assert!(!code(err).is_empty(), "{err:?}");
                }
            }
            "prefab.save" | "prefab.save_close" | "prefab.discard_close" => {
                // One shared editor for the three cases: ADR-7382 fixed the root leak that made a
                // prefab opened after `editor.save_prefab` and `editor.close_prefab { save: false }`
                // in the same world hold the earlier root too: a leak fails the next case's close
                // ("a prefab needs exactly one root entity") or its open (duplicate guids).
                let path = format!("assets/{}.prefab.ron", action.id.replace('.', "_"));
                agent.call("editor.create_asset", json!({ "kind": "prefab", "path": path }));
                agent.call("editor.open_prefab", json!({ "path": path }));
                assert!(agent.call("editor.prefab_mode", json!({}))["editing"].is_string());
                match action.id {
                    "prefab.save" => {
                        let out = agent.call("editor.save_prefab", json!({}));
                        assert_eq!(out["saved"], true, "{out}");
                        assert!(dir.join(&path).is_file());
                        agent.call("editor.close_prefab", json!({ "save": false }));
                    }
                    "prefab.save_close" => {
                        let out = agent.call("editor.close_prefab", json!({}));
                        assert_eq!(out["closed"], true, "{out}");
                    }
                    _ => {
                        let out = agent.call("editor.close_prefab", json!({ "save": false }));
                        assert_eq!(out["closed"], true, "{out}");
                    }
                }
                assert!(
                    agent.call("editor.prefab_mode", json!({}))["editing"].is_null(),
                    "{}",
                    action.id
                );
            }
            "recovery.restore" | "recovery.discard" => {
                let ActionPath::Command(id) = action.path else {
                    panic!("{}: a palette command", action.id);
                };
                agent.command(id);
                let want = if id == "recovery.restore" {
                    LayoutRequest::RestoreRecovery
                } else {
                    LayoutRequest::DiscardRecovery
                };
                // The shell's next frame performs it (`shell::tests::recovery_commands_*`).
                assert!(agent.layout_requests().contains(&want), "{id}");
            }
            "viewport.spacemouse_object" => {
                assert_eq!(
                    agent.command("viewport.toggle_spacemouse_object"),
                    "SpaceMouse Object tool on"
                );
                assert!(
                    agent
                        .world()
                        .resource::<regia_input::spacemouse::SpaceMouseNav>()
                        .object_tool
                );
            }
            "close.save_and_exit" | "close.discard_and_exit" | "close.cancel" => {
                // The prompt's choices: an agent saves (or not) and ends the editor with
                // `agent.shutdown { save }`; Cancel is the prompt's own (no state). Shutdown is
                // checked last, below, so the loop keeps an editor to drive.
            }
            "create.player_start"
            | "create.trigger_volume"
            | "create.point_light"
            | "create.spot_light"
            | "create.directional_light"
            | "create.camera"
            | "create.audio_emitter"
            | "create.rect_light"
            | "create.tube_light"
            | "create.decal"
            | "create.post_process_volume"
            | "create.spline"
            | "create.nav_link"
            | "create.smart_object" => {
                // Create ▸ <actor> is `editor.create_entity { kind }` (ADR-7468).
                let kind = action.id.trim_start_matches("create.");
                let out = agent.call("editor.create_entity", json!({ "kind": kind }));
                assert_eq!(out["name"], action.label, "{out}");
                assert!(agent.names().contains(&action.label.to_string()), "{kind}");
            }
            "create.reflection_capture" => {
                // Create ▸ Reflection capture is `editor.create_entity { kind }` (ADR-7592).
                let out = agent.call("editor.create_entity", json!({ "kind": "reflection_capture" }));
                assert_eq!(out["name"], action.label, "{out}");
                let entity = Entity::try_from_bits(out["entity"].as_u64().expect("id")).expect("entity");
                assert!(agent.world().get::<regia_render::LocalCapture>(entity).is_some());
            }
            "tools.bake_captures" => {
                // Tools ▸ Bake reflection captures is `editor.bake_captures` (ADR-7592): it starts
                // a job over the level's captures; a cancel ends it with nothing written.
                fn no_gpu(_: &mut World, _: u64) -> Result<(), String> {
                    Err("no GPU in this test".into())
                }
                let item = menu_item(action.id);
                assert_eq!(item.method, "editor.bake_captures");
                agent.world().init_resource::<regia_render::RuntimeRenderer>();
                agent
                    .world()
                    .insert_resource(regia_editor::capture_bake::CaptureBaker(no_gpu));
                agent.call("editor.create_entity", json!({ "kind": "reflection_capture" }));
                let out = agent.call(item.method, item.params.clone());
                assert_eq!(out["state"], "running", "{out}");
                agent.call("editor.cancel_task", json!({ "id": "bake_captures" }));
                regia_editor::capture_bake::tick(agent.world());
                let out = agent.call("editor.bake_captures_status", json!({}));
                assert_eq!(out["state"], "cancelled", "{out}");
                assert!(!dir.join("assets/maps/captures.fcap").exists());
            }
            "viewport.show_navigation" => {
                let out = agent.call("editor.set_viewport", json!({ "navigation": true }));
                assert_eq!(out["navigation"], true, "{out}");
                agent.call("editor.set_viewport", json!({ "navigation": false }));
            }
            "viewport.resolution_preview" => {
                // Toolbar ▸ Resolution is `editor.viewport_resolution_preview` (ADR-7744).
                let out = agent.call(
                    "editor.viewport_resolution_preview",
                    json!({ "preset": "hd_1080", "pane": [800, 600] }),
                );
                assert_eq!(out["on"], true, "{out}");
                assert_eq!(out["rect"], json!([0, 75, 800, 450]), "{out}");
                let out = agent.call("editor.viewport_resolution_preview", json!({ "off": true }));
                assert_eq!(out["on"], false, "{out}");
            }
            "viewport.show_colliders"
            | "viewport.show_light_radii"
            | "viewport.show_post_volumes"
            | "viewport.show_fog"
            | "viewport.show_ai_bt"
            | "viewport.show_ai_state_tree"
            | "viewport.show_ai_goap"
            | "viewport.show_ai_squad"
            | "viewport.show_ai_formation"
            | "viewport.show_ai_move_to"
            | "viewport.show_ai_blackboard" => {
                // Show ▸ Colliders / Light radii / Post volumes (ADR-7593), Fog of war (the fog-of-war debug
                // view, 12b624520) and the AI layers (ADR-8463) are `editor.set_viewport` flags.
                let flag = action.id.trim_start_matches("viewport.show_");
                let out = agent.call("editor.set_viewport", json!({ flag: true }));
                assert_eq!(out[flag], true, "{out}");
                agent.call("editor.set_viewport", json!({ flag: false }));
            }
            "viewport.nav_preview_cancel" => {
                // Show ▸ Cancel navigation preview is `editor.nav_preview { action: "cancel" }` (ADR-7664);
                // with no bake running it answers the state.
                let out = agent.call("editor.nav_preview", json!({ "action": "cancel" }));
                assert_eq!(out["building"], Value::Null, "{out}");
                assert_eq!(out["active"], false, "{out}");
            }
            "viewport.nav_preview_build" | "viewport.nav_preview_clear" => {
                // `editor.nav_preview { action }` (ADR-7593): this project's level has no static collision, so
                // a build is refused with its code and a clear finds nothing to remove.
                let out = agent.call("editor.nav_preview", json!({}));
                assert_eq!(out["active"], false, "{out}");
                if action.id == "viewport.nav_preview_build" {
                    let err = agent
                        .try_call("editor.nav_preview", json!({ "action": "build" }))
                        .expect_err("no static collision");
                    assert_eq!(code(&err), "nav_preview_no_geometry", "{err:?}");
                } else {
                    let out = agent.call("editor.nav_preview", json!({ "action": "clear" }));
                    assert_eq!(out["active"], false, "{out}");
                }
            }
            // The menus of ADR-7475 (`editor.menu_list` names each item's call).
            "file.new_script" => {
                let item = menu_item(action.id);
                let out = agent.call(item.method, item.params.clone());
                assert_eq!(out["open"], true, "{out}");
                agent.call(item.method, json!({ "open": false }));
            }
            // File ▸ Save all opens the Save all dialog; the menu names the same call, which opens
            // the same dialog (sprint-w12-rev-g10 row 3), and `editor.save_all {}` is its Save
            // selected with every row checked. `editor.menu_invoke "File/Save all"` saves the
            // dialog's savable documents too.
            "file.save_all" => {
                let item = menu_item(action.id);
                assert_eq!(
                    (item.method, item.params.clone()),
                    ("editor.save_all", json!({ "dialog": true })),
                    "the menu item and the click are one path"
                );
                agent.call(
                    "editor.save_level",
                    json!({ "path": "assets/maps/save_all.level.ron" }),
                );
                agent.call("editor.spawn", json!({ "name": "Unsaved for Save all" }));
                let out = agent.call(item.method, item.params.clone());
                assert_eq!(out["dialog"], true, "{out}");
                assert_eq!(out["documents"][0]["id"], "level", "{out}");
                assert_eq!(
                    agent
                        .world()
                        .get_resource::<regia_editor::save_all_dialog::SaveAllDialog>()
                        .and_then(|dialog| dialog.purpose.clone()),
                    Some(regia_editor::save_all_dialog::SavePurpose::SaveAll),
                    "the agent's call opened the dialog the click opens"
                );
                let out = agent.call("editor.save_all", json!({}));
                assert_eq!(out["saved"], json!(["level"]), "{out}");
                assert!(!agent.world().resource::<UndoStack>().is_dirty(), "{out}");
                agent.call("editor.spawn", json!({ "name": "Unsaved for menu_invoke" }));
                let out = agent.call("editor.menu_invoke", json!({ "path": "File/Save all" }));
                assert!(!out["level"].is_null(), "{out}");
                assert_eq!(out["failed"], json!([]), "{out}");
                assert!(out["saved"].is_array(), "the dialog's other documents: {out}");
                assert!(!agent.world().resource::<UndoStack>().is_dirty(), "{out}");
            }
            "file.recent" | "edit.cut" | "edit.copy" | "edit.paste" | "edit.duplicate" | "edit.delete"
            | "edit.select_all" => {
                let item = menu_item(action.id);
                assert_eq!(item.method, "editor.menu_invoke", "{}", action.id);
                if matches!(
                    action.id,
                    "edit.cut" | "edit.copy" | "edit.paste" | "edit.duplicate" | "edit.delete"
                ) {
                    let out = agent.call("editor.spawn", json!({ "name": format!("Menu {}", action.id) }));
                    agent.call("editor.select", json!({ "entities": [out["entity"]] }));
                }
                if action.id == "edit.paste" {
                    agent.call("editor.menu_invoke", json!({ "path": "Edit/Copy" }));
                }
                let out = agent.call(item.method, item.params.clone());
                assert_eq!(out["path"], item.params["path"], "{out}");
            }
            "file.exit" => {
                // `editor.menu_invoke { path: "File/Exit" }` asks the window to close (unsaved
                // changes ask first); the headless editor ends with `agent.shutdown`, checked last.
                let item = menu_item(action.id);
                assert_eq!(
                    (item.method, item.params),
                    ("editor.menu_invoke", json!({ "path": "File/Exit" }))
                );
            }
            // File ▸ Version history… (ADR-7928): the window lists `editor.versions`.
            "file.version_history" => {
                let out = agent.call("editor.versions", json!({}));
                assert!(out["versions"].is_array() && out["targets"].is_array(), "{out}");
            }
            // File ▸ Duplicate project… (ADR-7928): the folder dialog's choice is `dest`.
            "file.duplicate_project" => {
                std::fs::write(dir.join("Regia.toml"), "[project]\nname = \"Parity\"\n").expect("manifest");
                let dest = regia_test_utils::unique_temp_dir("agent-parity-duplicate").join("copy");
                let out = agent.call(
                    "editor.duplicate_project",
                    json!({ "dest": dest.display().to_string() }),
                );
                assert!(dest.join("Regia.toml").is_file(), "{out}");
                std::fs::remove_file(dir.join("Regia.toml")).expect("manifest");
                let _ = std::fs::remove_dir_all(dest.parent().expect("parent"));
            }
            "file.package" => {
                grant_build(&mut agent);
                let item = menu_item(action.id);
                assert_eq!(item.method, "editor.package_start");
                // This project has no Regia.toml: refused before anything runs.
                let err = agent
                    .try_call(item.method, item.params.clone())
                    .expect_err("no project to package");
                assert_eq!(reason(&err), "package_no_project", "{err:?}");
            }
            "tools.rebuild_asset_db" => {
                let err = agent
                    .try_call("editor.rebuild_asset_db", json!({}))
                    .expect_err("no Regia.toml");
                assert_eq!(reason(&err), "tools_no_project", "{err:?}");
            }
            "tools.clear_caches" | "tools.clear_caches_confirm" => {
                grant_build(&mut agent);
                let err = agent
                    .try_call("editor.clear_caches", json!({}))
                    .expect_err("asks first");
                assert_eq!(reason(&err), "needs_confirmation", "{err:?}");
                let err = agent
                    .try_call("editor.clear_caches", menu_item("tools.clear_caches").params)
                    .expect_err("no Regia.toml");
                assert_eq!(reason(&err), "package_no_project", "{err:?}");
            }
            "tools.clear_caches_cancel" => assert_eq!(action.path, ActionPath::ViewOnly),
            "tools.open_code_editor" => {
                // The project names its editor, so the host's $VISUAL/$EDITOR (the last fallback)
                // never runs: a build host that sets EDITOR=nvim would otherwise start a real
                // editor that outlives the test and holds the runner's inherited file descriptors.
                // The configured program is this test binary with `--list`, which exists on every
                // OS and exits at once. The refusal without any editor is the menus lib test
                // `open_code_editor_without_an_editor_is_refused`.
                let exe = std::env::current_exe().expect("test binary path");
                let exe = exe.to_string_lossy().into_owned();
                let config = dir.join(".regia/agent.toml");
                std::fs::create_dir_all(dir.join(".regia")).expect(".regia dir");
                std::fs::write(
                    &config,
                    format!("[editor]\ncode_editor = ['{exe}', \"--list\", \"{{file}}:{{line}}\"]\n"),
                )
                .expect("agent.toml");
                // Starting a process needs the build permission (w10-editor-agent-smalls (3)).
                grant_build(&mut agent);
                let out = agent.call("editor.open_code_editor", json!({}));
                std::fs::remove_file(&config).expect("remove agent.toml");
                assert_eq!(out["opened"], "code_editor", "{out}");
                assert_eq!(
                    out["command"][0],
                    exe.as_str(),
                    "the project's editor, not $EDITOR: {out}"
                );
                assert_eq!(out["command"][1], "--list", "{out}");
            }
            "play.simulate" | "play.players" => {
                let item = menu_item(action.id);
                agent.world().insert_resource(PieRequests::default());
                let out = agent.call(item.method, item.params.clone());
                assert_eq!(out["queued"], true, "{out}");
                assert_eq!(agent.world().resource::<PieRequests>().queue.len(), 1);
            }
            "help.documentation" | "help.agent_guide" => {
                let mut params = menu_item(action.id).params;
                params["dry_run"] = json!(true);
                let out = agent.call("editor.help_open", params);
                assert!(
                    std::path::Path::new(out["path"].as_str().unwrap_or_default()).is_file(),
                    "{out}"
                );
                assert_eq!(out["opened"], false);
            }
            // Help ▸ Samples… (w13): the window shows what `editor.samples { action: "list" }` answers.
            "help.samples" => {
                assert_eq!(action.path, ActionPath::Method("editor.samples"));
                let out = agent.call("editor.samples", json!({ "action": "list" }));
                let rows = out["samples"].as_array().expect("samples");
                assert!(rows.iter().any(|row| row["id"] == "template/blank"), "{out}");
                assert!(
                    rows.iter()
                        .all(|row| row["description"].as_str().is_some_and(|d| !d.is_empty())),
                    "{out}"
                );
            }
            "help.about" | "help.about_license" | "help.about_notices" => {
                let out = agent.call("editor.about", json!({}));
                let key = match action.id {
                    "help.about_license" => "license_path",
                    "help.about_notices" => "notices_path",
                    _ => "version",
                };
                assert!(out[key].is_string(), "{key}: {out}");
            }
            // Sequence ▸ Render movie… (ADR-7726): the dialog's job is `editor.render_movie`. The job
            // text comes back on a dry run; this world has no renderer, so a real start is refused by
            // `render.out_job` and starts nothing; an output folder outside the project is refused.
            "sequence.render_movie" => {
                let path = "assets/cine/parity_movie.seq.ron";
                if !dir.join(path).is_file() {
                    agent.call(
                        "editor.seq_new",
                        json!({ "path": path, "length": 1.0, "fps": 10.0 }),
                    );
                }
                let item = menu_item(action.id);
                assert_eq!(item.method, "editor.render_movie");
                let dry = agent.call(
                    "editor.render_movie",
                    json!({ "sequence": path, "out": "renders/parity", "width": 32, "height": 16, "dry_run": true }),
                );
                assert!(
                    dry["job_text"].as_str().is_some_and(|t| t.contains("movie")),
                    "{dry}"
                );
                let err = agent
                    .try_call(
                        "editor.render_movie",
                        json!({ "sequence": path, "out": "../outside" }),
                    )
                    .expect_err("outside the project");
                assert_eq!(reason(&err), "path_outside_project", "{err:?}");
                let started = agent.call(
                    "editor.render_movie",
                    json!({ "sequence": path, "out": "renders/parity" }),
                );
                assert_eq!(started["task"], "render_movie", "{started}");
                let err = agent
                    .try_call(
                        "editor.render_movie",
                        json!({ "sequence": path, "out": "renders/parity" }),
                    )
                    .expect_err("one render at a time");
                assert_eq!(reason(&err), "render_movie_busy", "{err:?}");
                // This world has no renderer: the run ends by itself (failed) or on the cancel.
                let _ = agent.try_call("editor.cancel_task", json!({ "id": "render_movie" }));
                let mut state = Value::Null;
                for _ in 0..600 {
                    state = agent.call("editor.render_movie_status", json!({}))["state"].clone();
                    if state != "running" {
                        break;
                    }
                }
                assert!(state == "cancelled" || state == "failed", "{state}");
            }
            // The viewport toolbar's Surface snap / Align to surface toggles (ADR-7756) are
            // `editor.placement_options { surface_snap | surface_align }`; the answer carries both.
            "viewport.surface_snap" | "viewport.surface_align" => {
                assert_eq!(action.path, ActionPath::Method("editor.placement_options"));
                let key = action.id.trim_start_matches("viewport.");
                let on = agent.call("editor.placement_options", json!({ key: true }));
                assert_eq!(on[key], true, "{on}");
                let read = agent.call("editor.placement_options", json!({}));
                assert_eq!(read[key], true, "the toggle stays on: {read}");
                let off = agent.call("editor.placement_options", json!({ key: false }));
                assert_eq!(off[key], false, "{off}");
            }
            // Camera navigation toggles of the viewport toolbar (ADR-7755): `editor.set_viewport`
            // flags, flipped and put back.
            "viewport.orbit_selection" | "viewport.zoom_to_cursor" | "viewport.speed_with_distance" => {
                assert_eq!(action.path, ActionPath::Method("editor.set_viewport"));
                let flag = action.id.trim_start_matches("viewport.");
                let was = agent.call("editor.set_viewport", json!({}))[flag]
                    .as_bool()
                    .expect("a boolean flag");
                let out = agent.call("editor.set_viewport", json!({ flag: !was }));
                assert_eq!(out[flag], !was, "{out}");
                let out = agent.call("editor.set_viewport", json!({ flag: was }));
                assert_eq!(out[flag], was, "{out}");
            }
            "viewport.invert_orbit_x"
            | "viewport.invert_orbit_y"
            | "viewport.invert_pan_x"
            | "viewport.invert_pan_y"
            | "viewport.invert_fly_y"
            | "viewport.invert_wheel" => {
                assert_eq!(action.path, ActionPath::Method("editor.set_viewport"));
                let axis = action.id.trim_start_matches("viewport.invert_");
                let out = agent.call("editor.set_viewport", json!({ "invert": { axis: true } }));
                assert_eq!(out["invert"][axis], true, "{out}");
                let others = out["invert"]
                    .as_object()
                    .expect("invert object")
                    .iter()
                    .filter(|(name, on)| name.as_str() != axis && on.as_bool() == Some(true))
                    .count();
                assert_eq!(others, 0, "only {axis} is inverted: {out}");
                let out = agent.call("editor.set_viewport", json!({ "invert": { axis: false } }));
                assert_eq!(out["invert"][axis], false, "{out}");
            }
            // Camera and object (ADR-7755): the toolbar's Camera menu and the View menu run the same
            // palette commands on the perspective camera.
            "viewport.camera_to_selection" | "view.camera_to_selection" => {
                assert_eq!(action.path, ActionPath::Command("view.camera_to_selection"));
                let out = agent.call(
                    "editor.spawn",
                    json!({ "name": "Lookout", "translation": [3.0, 4.0, 5.0] }),
                );
                let entity = out["entity"].as_u64().expect("entity");
                agent.call("editor.select", json!({ "entities": [entity] }));
                let before = agent.undo_depth();
                assert_eq!(
                    agent.command("view.camera_to_selection"),
                    "Moved the camera to the selection"
                );
                let eye = agent.world().resource::<EditorCamera>().eye();
                assert!(
                    (eye - regia_core::math::Vec3::new(3.0, 4.0, 5.0)).length() < 1e-3,
                    "{eye:?}"
                );
                assert_eq!(agent.undo_depth(), before, "a camera move is not an edit");
            }
            "viewport.selection_to_camera" | "view.selection_to_camera" => {
                assert_eq!(action.path, ActionPath::Command("view.selection_to_camera"));
                let out = agent.call("editor.spawn", json!({ "name": "Carried" }));
                let bits = out["entity"].as_u64().expect("entity");
                agent.call("editor.select", json!({ "entities": [bits] }));
                let before = agent.undo_depth();
                assert_eq!(
                    agent.command("view.selection_to_camera"),
                    "Moved the selection to the camera"
                );
                assert_eq!(agent.undo_depth(), before + 1, "one undo step");
                let eye = agent.world().resource::<EditorCamera>().eye();
                let entity = Entity::try_from_bits(bits).expect("entity");
                let at = agent
                    .world()
                    .get::<regia_scene::prelude::Transform>(entity)
                    .expect("transform")
                    .translation;
                assert!((at - eye).length() < 1e-3, "{at:?} vs {eye:?}");
            }
            // Pilot, look through, the camera preview and the guides (ADR-7768).
            "viewport.pilot" | "viewport.eject" | "viewport.pilot_camera" => {
                let transform = regia_scene::prelude::Transform::from_translation(
                    regia_core::math::Vec3::new(1.0, 2.0, 3.0),
                );
                let light = agent
                    .world()
                    .spawn((transform, regia_render::PointLight::default()))
                    .id();
                let before = agent.undo_depth();
                let out = agent.call("editor.pilot", json!({ "entity": light.to_bits() }));
                assert_eq!(out["mode"], "pilot", "{out}");
                let out = agent.call("editor.pilot", json!({ "fly": { "forward": 1.0, "steps": 3 } }));
                assert_eq!(out["mode"], "pilot", "{out}");
                let out = agent.call("editor.pilot", json!({ "entity": null }));
                assert_eq!(out["mode"], Value::Null, "{out}");
                assert_eq!(agent.undo_depth(), before + 1, "one undo step per pilot session");
            }
            "viewport.look_through" | "viewport.editor_camera" => {
                let transform = regia_scene::prelude::Transform::from_translation(
                    regia_core::math::Vec3::new(0.0, 2.0, 8.0),
                );
                let camera = agent
                    .world()
                    .spawn((transform, regia_render::Camera::default()))
                    .id();
                let listed = agent.call("editor.cameras", json!({}));
                assert!(
                    listed["cameras"].as_array().is_some_and(|rows| rows
                        .iter()
                        .any(|r| r["entity"].as_u64() == Some(camera.to_bits()))),
                    "{listed}"
                );
                let out = agent.call("editor.look_through", json!({ "entity": camera.to_bits() }));
                assert_eq!(out["mode"], "look_through", "{out}");
                let out = agent.call("editor.look_through", json!({ "entity": null }));
                assert_eq!(out["mode"], Value::Null, "{out}");
            }
            "viewport.camera_preview" => {
                let out = agent.call("editor.camera_preview", json!({ "size": "small" }));
                assert_eq!(out["size"], "small", "{out}");
                agent.call("editor.camera_preview", json!({ "size": "medium" }));
            }
            "viewport.guide_thirds"
            | "viewport.guide_center"
            | "viewport.guide_safe_areas"
            | "viewport.guide_letterbox" => {
                let (key, value) = match action.id {
                    "viewport.guide_thirds" => ("thirds", json!(true)),
                    "viewport.guide_center" => ("center", json!(true)),
                    "viewport.guide_safe_areas" => ("safe_areas", json!(true)),
                    _ => ("letterbox", json!("16:9")),
                };
                let out = agent.call("editor.set_viewport", json!({ "guides": { key: value } }));
                assert!(
                    out["guides"][key] != json!(false) && !out["guides"][key].is_null(),
                    "{out}"
                );
            }
            "file.screenshot" => {
                assert_eq!(action.path, ActionPath::Command("file.screenshot"));
                assert_eq!(agent.command("file.screenshot"), "Screenshot…");
                assert!(
                    agent
                        .world()
                        .resource::<regia_editor::screenshot::ScreenshotDialog>()
                        .open
                );
            }
            // View ▸ Bulk Edit (ADR-7787): `editor.bulk_edit_open` on the selection opens the grid
            // of the selected entities by a component they share and brings the panel forward.
            "view.bulk_edit" => {
                let item = menu_item(action.id);
                assert_eq!(item.method, "editor.bulk_edit_open");
                let mut ids = Vec::new();
                for name in ["Grid A", "Grid B"] {
                    let out = agent.call("editor.spawn", json!({ "name": name }));
                    ids.push(out["entity"].as_u64().expect("entity"));
                }
                // `mode: "add"` after a clear: the default "set" keeps only the last id today
                // (reported to the lead in the w12-bulk-edit-grid report).
                agent.call("editor.select", json!({ "entities": [] }));
                agent.call("editor.select", json!({ "entities": ids, "mode": "add" }));
                let out = agent.call(item.method, item.params.clone());
                assert_eq!(out["rows_total"], 2, "{out}");
                assert!(
                    agent
                        .layout_requests()
                        .contains(&LayoutRequest::Open(Panel::BulkEdit))
                );
            }
            "tools.project_health" => {
                // Tools ▸ Project Health brings the panel's tab forward (ADR-7789).
                let ActionPath::Command(id) = action.path else {
                    panic!("{}: a palette command", action.id);
                };
                agent.command(id);
                assert!(
                    agent
                        .layout_requests()
                        .contains(&LayoutRequest::Open(Panel::ProjectHealth)),
                    "{}",
                    action.id
                );
            }
            // Edit ▸ Prefab overrides… and Edit ▸ Edit prefab in place (ADR-7790): the palette
            // commands on the selected instance. In place, a level save is refused (ADR-7835).
            "edit.prefab_overrides" | "edit.prefab_in_place" => {
                let ActionPath::Command(id) = action.path else {
                    panic!("{}: a palette command", action.id);
                };
                let source = agent.call("editor.spawn", json!({ "name": "Overrides source" }))["entity"]
                    .as_u64()
                    .expect("entity");
                let path = format!("assets/{}.prefab.ron", action.id.replace('.', "_"));
                let instance = agent.call(
                    "editor.create_prefab",
                    json!({ "entities": [source], "path": path }),
                )["instance"]
                    .as_u64()
                    .expect("instance");
                agent.call("editor.select", json!({ "entities": [instance] }));
                let message = agent.command(id);
                if id == "prefab.overrides" {
                    assert_eq!(message, "Prefab overrides shown");
                    let window = agent
                        .world()
                        .resource::<regia_editor::prefab_overrides_panel::PrefabOverridesWindow>()
                        .clone();
                    assert!(window.open && window.entity == Some(instance), "{window:?}");
                } else {
                    assert!(message.starts_with("Editing "), "{message}");
                    assert_eq!(agent.call("editor.prefab_mode", json!({}))["in_context"], true);
                    let err = agent
                        .try_call(
                            "editor.save_level",
                            json!({ "path": "assets/maps/in_place.level.ron" }),
                        )
                        .expect_err("refused while open in place");
                    assert_eq!(reason(&err), "prefab_in_place", "{err:?}");
                    assert!(!dir.join("assets/maps/in_place.level.ron").exists());
                    agent.call("editor.close_prefab", json!({ "save": false }));
                    assert!(agent.call("editor.prefab_mode", json!({}))["editing"].is_null());
                }
            }
            // File ▸ New level variant… (ADR-7823): `editor.new_level_variant` with the base and path the
            // two OS dialogs pick; a base that does not exist answers `base_missing`.
            "file.new_level_variant" => {
                assert_eq!(action.path, ActionPath::OsDialog("editor.new_level_variant"));
                let err = agent
                    .try_call(
                        "editor.new_level_variant",
                        json!({ "base": "assets/maps/none.level.ron", "path": "assets/maps/none.levelvariant.ron" }),
                    )
                    .expect_err("no base");
                assert_eq!(reason(&err), "base_missing", "{err:?}");
            }
            // ADR-7829: the play toolbar's start menu, speed/mute, view switch and device menu.
            "play.start_player_start" | "play.start_camera" | "play.start_entity" => {
                agent.world().insert_resource(PieRequests::default());
                let start = match action.id {
                    "play.start_player_start" => "player_start",
                    "play.start_camera" => "camera",
                    _ => "entity",
                };
                let out = agent.try_call("editor.play", json!({ "start": start }));
                if start == "entity" {
                    // This level has no player start to name.
                    let error = out.expect_err("no player start");
                    let reason = error
                        .data
                        .as_ref()
                        .and_then(|d| d["reason"].as_str().map(str::to_owned));
                    assert_eq!(reason.as_deref(), Some("play_start_not_player_start"));
                } else {
                    let out = out.expect("queued");
                    assert_eq!(out["start"]["kind"], start, "{out}");
                    assert_eq!(agent.world().resource::<PieRequests>().queue.len(), 1);
                }
            }
            "play.mute" => {
                assert!(
                    agent
                        .try_call("editor.play_controls", json!({ "mute": true }))
                        .is_err(),
                    "not playing"
                );
                agent.world().insert_resource(regia_editor::pie::PieStatus {
                    mode: Some(regia_editor::pie::PlayMode::Playing),
                    ..Default::default()
                });
                agent.world().insert_resource(PieRequests::default());
                let out = agent.call("editor.play_controls", json!({ "mute": true }));
                assert_eq!(out["queued"], true, "{out}");
                let queue = agent.world().resource::<PieRequests>().queue.clone();
                assert_eq!(queue, vec![PieRequest::SetMute(true)]);
                agent
                    .world()
                    .insert_resource(regia_editor::pie::PieStatus::default());
            }
            "play.view_server" | "play.view_client" => {
                // Headless play has no client worlds: one world, and the switch says so.
                let error = agent
                    .try_call("editor.pie_view", json!({ "role": "server" }))
                    .expect_err("no client worlds");
                let code = error
                    .data
                    .as_ref()
                    .and_then(|d| d["code"].as_str().map(str::to_owned));
                assert_eq!(code.as_deref(), Some("pie_net_no_client_worlds"));
            }
            "play.device"
            | "play.device_rotate"
            | "play.device_touch"
            | "play.device_gamepad"
            | "play.device_safe_area" => {
                let params = json!({ "window": true, "device": { "preset": "phone", "landscape": true, "touch": true, "gamepad": true, "safe_area_outline": true } });
                assert!(
                    agent.try_call("editor.play", params.clone()).is_err(),
                    "no window system headless"
                );
                agent
                    .world()
                    .insert_resource(regia_editor::play_window::PlayWindowSupport);
                agent.world().insert_resource(PieRequests::default());
                let out = agent.call("editor.play", params);
                assert_eq!(out["window"], "play_window", "{out}");
                let sim = *agent.world().resource::<regia_editor::play_device::DeviceSim>();
                assert!(
                    sim.landscape
                        && sim.touch
                        && sim.gamepad
                        && sim.safe_area_outline
                        && sim.preset.is_some()
                );
                agent
                    .world()
                    .remove_resource::<regia_editor::play_window::PlayWindowSupport>();
                agent
                    .world()
                    .remove_resource::<regia_editor::play_device::DeviceSim>();
            }
            // Window ▸ Platforms / Launch Profiles / Size Report (3261): each opens through its
            // method's `open` and answers its idle state.
            "platforms.open" => {
                assert_eq!(action.path, ActionPath::Method("editor.platform_check"));
                let out = agent.call("editor.platform_check", json!({ "open": true }));
                assert_eq!(out["open"], true, "{out}");
                assert_eq!(out["state"], "idle", "{out}");
                assert!(
                    agent
                        .world()
                        .resource::<regia_editor::platforms_panel::PlatformsPanel>()
                        .open
                );
                let out = agent.call("editor.platform_check", json!({ "open": false }));
                assert_eq!(out["open"], false, "{out}");
            }
            "launch_profiles.open" => {
                assert_eq!(action.path, ActionPath::Method("editor.launch_profiles"));
                let out = agent.call("editor.launch_profiles", json!({ "open": true }));
                assert_eq!(out["format_version"], 1, "{out}");
                assert!(out["profiles"].is_array(), "{out}");
                assert!(
                    agent
                        .world()
                        .resource::<regia_editor::launch_profiles::ProfilesPanel>()
                        .open
                );
                agent.call("editor.launch_profiles", json!({ "open": false }));
            }
            "size_report.open" => {
                assert_eq!(action.path, ActionPath::Method("editor.package_size_report"));
                let out = agent.call("editor.package_size_report", json!({ "open": true }));
                assert_eq!(out["open"], true, "{out}");
                assert_eq!(out["state"], "idle", "{out}");
                let out = agent.call("editor.package_size_report", json!({ "open": false }));
                assert_eq!(out["open"], false, "{out}");
            }
            // Window ▸ Undo History (3255), Notes and Activity (3259): the generated
            // `window.open_<panel>` commands bring the tab forward.
            "window.undo_history" | "window.notes" | "window.activity" => {
                let ActionPath::Command(id) = action.path else {
                    panic!("{}: a palette command", action.id);
                };
                let want = match action.id {
                    "window.undo_history" => Panel::UndoHistory,
                    "window.notes" => Panel::Notes,
                    _ => Panel::Activity,
                };
                assert_eq!(id, format!("window.open_{}", want.id()), "{}", action.id);
                agent.command(id);
                assert!(
                    agent.layout_requests().contains(&LayoutRequest::Open(want)),
                    "{id}"
                );
            }
            // Viewport toolbar ▸ Kit painting (3236): `editor.kit_paint { enabled }` takes the
            // viewport pointer and turns it back.
            "kit.paint_mode" => {
                assert_eq!(action.path, ActionPath::Method("editor.kit_paint"));
                allow_heavy(&mut agent);
                let out = agent.call("editor.kit_paint", json!({ "enabled": true }));
                assert_eq!(out["enabled"], true, "{out}");
                assert!(
                    agent
                        .world()
                        .resource::<regia_editor::kit_paint::KitPaint>()
                        .active
                );
                let out = agent.call("editor.kit_paint", json!({ "enabled": false }));
                assert_eq!(out["enabled"], false, "{out}");
            }
            // Edit ▸ Paste as child / Paste at original position: the menu items run through
            // `editor.menu_invoke` and paste the clipboard's copy.
            "edit.paste_as_child" | "edit.paste_at_original" => {
                let item = menu_item(action.id);
                assert_eq!(item.method, "editor.menu_invoke", "{}", action.id);
                let base = format!("Paste {}", action.id);
                let out = agent.call("editor.spawn", json!({ "name": base }));
                agent.call("editor.select", json!({ "entities": [out["entity"]] }));
                agent.call("editor.menu_invoke", json!({ "path": "Edit/Copy" }));
                let count = |agent: &mut Agent| {
                    agent
                        .names()
                        .iter()
                        .filter(|n| n.starts_with(base.as_str()))
                        .count()
                };
                let before = count(&mut agent);
                let out = agent.call(item.method, item.params.clone());
                assert_eq!(out["path"], item.params["path"], "{out}");
                assert_eq!(count(&mut agent), before + 1, "{} pasted one copy", action.id);
            }
            // Edit ▸ Select similar (3210): the palette commands select every entity of the same
            // kind, components, asset or layer as the selection.
            "edit.select_similar_kind"
            | "edit.select_similar_components"
            | "edit.select_similar_asset"
            | "edit.select_similar_layer" => {
                let ActionPath::Command(id) = action.path else {
                    panic!("{}: a palette command", action.id);
                };
                agent.call("editor.select", json!({ "entities": [] }));
                assert_eq!(agent.command(id), "Nothing selected to compare with", "{id}");
                // Two point lights share a kind (a plain entity has none and matches only itself).
                let a = agent
                    .world()
                    .spawn((
                        regia_scene::prelude::Name::new("Similar A"),
                        regia_render::PointLight::default(),
                    ))
                    .id()
                    .to_bits();
                agent.world().spawn((
                    regia_scene::prelude::Name::new("Similar B"),
                    regia_render::PointLight::default(),
                ));
                agent.call("editor.select", json!({ "entities": [a] }));
                let message = agent.command(id);
                assert!(message.starts_with("Selected "), "{id}: {message}");
                assert!(!agent.world().resource::<Selection>().is_empty(), "{id}");
                if id == "selection.similar_kind" {
                    assert!(
                        agent.world().resource::<Selection>().entities().len() >= 2,
                        "both lights share a kind: {message}"
                    );
                }
            }
            // Viewport ▸ the pick stack's row (3210): `editor.pick_stack { x, y, cpu }` lists what
            // is under the cursor; a row past the list is refused.
            "viewport.pick_stack_row" => {
                assert_eq!(action.path, ActionPath::Method("editor.pick_stack"));
                let out = agent.call("editor.pick_stack", json!({ "x": 160, "y": 120, "cpu": true }));
                assert_eq!(out["method"], "cpu", "{out}");
                let rows = out["entities"].as_array().map_or(0, Vec::len);
                assert!(
                    agent
                        .try_call(
                            "editor.pick_stack",
                            json!({ "x": 160, "y": 120, "cpu": true, "select": rows + 1 }),
                        )
                        .is_err(),
                    "a row past the stack"
                );
            }
            // View ▸ Rendering Debug (3211) and View ▸ Asset Jobs (3224): window toggles.
            "view.rendering_debug" => {
                assert_eq!(action.path, ActionPath::Command("view.toggle_rendering_debug"));
                assert_eq!(
                    agent.command("view.toggle_rendering_debug"),
                    "Rendering Debug shown"
                );
                assert!(
                    agent
                        .world()
                        .resource::<regia_editor::render_debug_panel::RenderDebugPanel>()
                        .open
                );
                assert_eq!(
                    agent.command("view.toggle_rendering_debug"),
                    "Rendering Debug hidden"
                );
            }
            "view.asset_jobs" => {
                assert_eq!(action.path, ActionPath::Command("window.asset_jobs"));
                assert_eq!(agent.command("window.asset_jobs"), "Asset Jobs shown");
                assert!(
                    agent
                        .world()
                        .resource::<regia_editor::asset_jobs_panel::AssetJobsPanel>()
                        .open
                );
                assert_eq!(agent.command("window.asset_jobs"), "Asset Jobs hidden");
            }
            // Viewport toolbar ▸ Show ▸ Paths (3211): `editor.set_viewport { paths }`.
            "viewport.show_paths" => {
                assert_eq!(action.path, ActionPath::Method("editor.set_viewport"));
                let out = agent.call("editor.set_viewport", json!({ "paths": true }));
                assert_eq!(out["paths"], true, "{out}");
                let out = agent.call("editor.set_viewport", json!({ "paths": false }));
                assert_eq!(out["paths"], false, "{out}");
            }
            // Show ▸ Lockstep (ADR-8562): `editor.set_viewport { lockstep }`.
            "viewport.show_lockstep" => {
                assert_eq!(action.path, ActionPath::Method("editor.set_viewport"));
                let out = agent.call("editor.set_viewport", json!({ "lockstep": true }));
                assert_eq!(out["lockstep"], true, "{out}");
                let out = agent.call("editor.set_viewport", json!({ "lockstep": false }));
                assert_eq!(out["lockstep"], false, "{out}");
            }
            // View ▸ Color Vision (3264): `editor.color_vision { mode, deficiency }` filters the
            // chrome and the viewport; an unknown deficiency is refused.
            "view.color_vision" => {
                assert_eq!(action.path, ActionPath::Method("editor.color_vision"));
                let out = agent.call(
                    "editor.color_vision",
                    json!({ "mode": "simulate", "deficiency": "deuteranopia" }),
                );
                assert_eq!(out["mode"], "simulate", "{out}");
                assert_eq!(out["active"], true, "{out}");
                let err = agent
                    .try_call("editor.color_vision", json!({ "deficiency": "nope" }))
                    .expect_err("an unknown deficiency");
                assert_eq!(code(&err), "color_vision_bad_deficiency", "{err:?}");
                let out = agent.call("editor.color_vision", json!({ "mode": "off" }));
                assert_eq!(out["active"], false, "{out}");
            }
            "library.list" | "library.add" | "library.use" => library_shell_action(action),
            // Prefab banner ▸ Open graph / New graph for this prefab (P-165, ADR-8566).
            "prefab.logic_open" | "prefab.logic_new" => {
                assert_eq!(action.path, ActionPath::Method("editor.prefab_logic"));
                let path = format!("assets/{}.prefab.ron", action.id.replace('.', "_"));
                agent.call("editor.create_asset", json!({ "kind": "prefab", "path": path }));
                agent.call("editor.open_prefab", json!({ "path": path }));
                let err = agent
                    .try_call("editor.prefab_logic", json!({ "op": "open" }))
                    .expect_err("no graph yet");
                // book/agents/prefab-logic.md: errors are `F0002` with the specific code in `data.reason`.
                assert_eq!(code(&err), "F0002", "{err:?}");
                assert_eq!(reason(&err), "prefab_logic_none", "{err:?}");
                let out = agent.call("editor.prefab_logic", json!({ "op": "new" }));
                assert_eq!(out["created"], true, "{out}");
                let graph = out["path"].as_str().expect("path").to_string();
                assert!(
                    graph.ends_with(".logic.ron") && dir.join(&graph).is_file(),
                    "{out}"
                );
                assert!(out["undoable"].as_u64().is_some_and(|n| n >= 1), "{out}");
                if action.id == "prefab.logic_open" {
                    let out = agent.call("editor.prefab_logic", json!({ "op": "open" }));
                    assert_eq!(out["path"], graph.as_str(), "{out}");
                    assert_eq!(out["created"], false, "{out}");
                }
                let err = agent
                    .try_call("editor.prefab_logic", json!({ "op": "nope" }))
                    .expect_err("a bad op");
                assert_eq!(code(&err), "F0002", "{err:?}");
                assert_eq!(reason(&err), "prefab_logic_bad_op", "{err:?}");
                agent.call("editor.close_prefab", json!({ "save": false }));
            }
            other => panic!("shell action {other} has no headless check here"),
        }
    }
    let out = agent.call("agent.shutdown", json!({ "save": false }));
    assert_eq!(out["exiting"], true, "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The Inspector's prefab-override Revert and Apply (`editor.prefab_override`) over the dispatcher:
/// a plain entity has no override (`not_prefab_instance`), an unknown action is refused.
#[test]
fn prefab_override_refuses_a_plain_entity_and_a_bad_action() {
    let dir = project();
    let mut agent = Agent::new(&dir);
    let entity = agent.call("editor.spawn", json!({ "name": "Plain" }))["entity"]
        .as_u64()
        .expect("entity");
    let err = agent
        .try_call(
            "editor.prefab_override",
            json!({ "entity": entity, "component": "Transform", "path": "translation", "action": "revert" }),
        )
        .expect_err("a plain entity is not a prefab instance");
    assert_eq!(code(&err), "not_prefab_instance", "{err:?}");
    let err = agent
        .try_call(
            "editor.prefab_override",
            json!({ "entity": entity, "component": "Transform", "path": "translation", "action": "keep" }),
        )
        .expect_err("unknown action");
    assert_eq!(code(&err), "bad_action", "{err:?}");
    let err = agent
        .try_call(
            "editor.prefab_override",
            json!({ "entity": "x", "component": "T", "path": "p", "action": "apply" }),
        )
        .expect_err("bad entity");
    assert_eq!(code(&err), "bad_entity", "{err:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The registry's panel actions outside the shell (the Material panel, the Terrain panel) run
/// headless through their `editor.command` ids. A new registry action without a case here fails.
#[test]
fn every_panel_action_runs_headless() {
    let dir = project();
    let mut agent = Agent::new(&dir);
    for action in ACTIONS
        .iter()
        .filter(|a| !(a.panel.starts_with("shell") || a.panel.starts_with("viewport")))
    {
        match action.id {
            "agent.pick" | "agent.plan_check" | "agent.staged_check" => {
                assert_eq!(action.path, ActionPath::ViewOnly);
            }
            // Frame Debugger ▸ Capture frame / Resume (3253): `editor.frame_capture { action }`. With
            // no viewport render headless, a start stays pending and a resume releases it.
            "frame_debugger.capture" | "frame_debugger.resume" => {
                assert_eq!(action.path, ActionPath::Method("editor.frame_capture"));
                let out = agent.call("editor.frame_capture", json!({ "action": "start" }));
                assert_eq!(out["state"], "pending", "{out}");
                let again = agent.call("editor.frame_capture", json!({ "action": "start" }));
                assert_eq!(
                    again["pane"], out["pane"],
                    "a repeated start is idempotent: {again}"
                );
                let out = agent.call("editor.frame_capture", json!({ "action": "resume" }));
                assert_eq!(out["state"], "idle", "{out}");
            }
            // Samples window ▸ Open and an extension sample's Open (w13): `editor.samples { action:
            // "create" }` copies the entry and opens the copy when asked.
            "samples.open" | "extensions.sample_open" => {
                assert_eq!(action.path, ActionPath::Method("editor.samples"));
                let parent = regia_test_utils::unique_temp_dir("parity-samples");
                let out = agent.call(
                    "editor.samples",
                    json!({ "action": "create", "id": "template/blank", "parent": parent.display().to_string(), "name": "parity_copy" }),
                );
                assert_eq!(out["project"], true, "{out}");
                assert!(
                    std::path::Path::new(out["root"].as_str().expect("root"))
                        .join("Regia.toml")
                        .is_file(),
                    "{out}"
                );
            }
            // Frame Debugger ▸ Capture play window (ADR-8224): `editor.frame_capture { action: "start",
            // source: "play_window" }`. Headless no play window is open: refused, nothing pending.
            "frame_debugger.capture_play_window" => {
                assert_eq!(action.path, ActionPath::Method("editor.frame_capture"));
                let err = agent
                    .try_call(
                        "editor.frame_capture",
                        json!({ "action": "start", "source": "play_window" }),
                    )
                    .expect_err("no play window");
                assert_eq!(
                    err.data.as_ref().map(|data| data["code"].clone()),
                    Some(json!("capture_no_play_window")),
                    "{err:?}"
                );
                let out = agent.call("editor.frame_capture", json!({ "action": "state" }));
                assert_ne!(out["state"], "pending", "{out}");
            }
            // Frame Debugger ▸ Previous draw / Next draw (ADR-8224): `editor.frame_draw { step }`.
            // Headless no frame was captured: the step is refused with frame_capture_missing and
            // the selection is left alone.
            "frame_debugger.previous_draw" | "frame_debugger.next_draw" => {
                assert_eq!(action.path, ActionPath::Method("editor.frame_draw"));
                let step = if action.id == "frame_debugger.next_draw" {
                    "next"
                } else {
                    "previous"
                };
                let err = agent
                    .try_call("editor.frame_draw", json!({ "step": step }))
                    .expect_err("no capture to step through");
                assert_eq!(
                    err.data.as_ref().map(|data| data["code"].clone()),
                    Some(json!("frame_capture_missing")),
                    "{err:?}"
                );
            }
            "agent.preferences" => {
                agent.command("window.preferences");
                assert!(
                    agent
                        .layout_requests()
                        .contains(&LayoutRequest::Open(Panel::Preferences))
                );
            }
            "agent.start"
            | "agent.stop"
            | "agent.send"
            | "agent.mode"
            | "agent.approve"
            | "agent.reject"
            | "agent.staged_approve_all"
            | "agent.staged_approve"
            | "agent.staged_reject"
            | "agent.staged_reject_all" => {
                agent
                    .world()
                    .resource_mut::<regia_agent::AgentPermissions>()
                    .config
                    .permissions
                    .insert("tools".into());
                let params = match action.id {
                    "agent.start" => json!({"action":"start", "command":"Unconfigured"}),
                    "agent.stop" => json!({"action":"stop", "session":0}),
                    "agent.mode" => json!({"action":"mode", "session":0, "mode":"ask"}),
                    "agent.send" => json!({"action":"send", "session":0, "text":"Hello"}),
                    // P-369: staged-call decisions (approve/reject, all or the checked items).
                    "agent.staged_approve_all" | "agent.staged_reject_all" => json!({
                        "action":"staged", "session":0, "approve":action.id == "agent.staged_approve_all"
                    }),
                    "agent.staged_approve" | "agent.staged_reject" => json!({
                        "action":"staged", "session":0, "approve":action.id == "agent.staged_approve", "items":[1]
                    }),
                    _ => json!({"action":"send", "session":0, "approve_plan":action.id == "agent.approve"}),
                };
                let error = agent
                    .try_call("editor.agent_sessions", params)
                    .expect_err("no configured session");
                assert_eq!(
                    code(&error),
                    if action.id == "agent.start" {
                        "session_not_configured"
                    } else {
                        "session_not_found"
                    }
                );
            }

            "material.undo" | "material.redo" | "material.reload" => {
                let ActionPath::Command(id) = action.path else {
                    panic!("{}: a palette command", action.id);
                };
                agent.command(id);
                let want = match id {
                    "material.undo" => LayoutRequest::MaterialUndo,
                    "material.redo" => LayoutRequest::MaterialRedo,
                    _ => LayoutRequest::MaterialReload,
                };
                // The shell's next frame applies it to the panel (`shell::tests::material_commands_*`).
                assert!(agent.layout_requests().contains(&want), "{id}");
            }
            "terrain.retry_level_terrain" => {
                agent
                    .world()
                    .insert_resource(regia_editor::terrain_panel::EditorLevelTerrain {
                        error: Some(("terrain_missing".into(), "no heightfield".into())),
                        ..Default::default()
                    });
                assert_eq!(
                    agent.command("terrain.retry_level_terrain"),
                    "Level terrain: loading again"
                );
                assert!(
                    agent
                        .world()
                        .get_resource::<regia_editor::terrain_panel::EditorLevelTerrain>()
                        .is_none_or(|t| t.error.is_none()),
                    "the failed load is dropped so the editor loads again"
                );
            }
            "outliner.create_prefab" | "outliner.create_prefab_confirm" => {
                // The row menu item opens the dialog; its Create is `editor.create_prefab` (ADR-7468).
                let entity = agent.call("editor.spawn", json!({ "name": "Prefab source" }))["entity"]
                    .as_u64()
                    .expect("entity");
                let path = format!("assets/{}.prefab.ron", action.id.replace('.', "_"));
                let out = agent.call(
                    "editor.create_prefab",
                    json!({ "entities": [entity], "path": path }),
                );
                assert!(out["instance"].is_u64(), "{out}");
                assert!(dir.join(&path).is_file(), "{path}");
            }
            // The dialog's Cancel closes it (no state).
            "outliner.create_prefab_cancel" => {}
            // Version History ▸ Preview and Restore (ADR-7928): `editor.version_preview` /
            // `editor.version_restore` on a version the autosave kept.
            "versions.preview" | "versions.restore" => {
                agent.call(
                    "editor.save_level",
                    json!({ "path": "assets/maps/versions.level.ron" }),
                );
                agent.call("editor.spawn", json!({ "name": "Versioned" }));
                agent.call("editor.autosave_now", json!({}));
                let listed = agent.call("editor.versions", json!({}));
                let id = listed["versions"][0]["id"]
                    .as_str()
                    .expect("a version")
                    .to_owned();
                if action.id == "versions.preview" {
                    let shown = agent.call("editor.version_preview", json!({ "id": id }));
                    assert!(shown["entities"].as_array().is_some(), "{shown}");
                } else {
                    let out = agent.call("editor.version_restore", json!({ "id": id }));
                    assert_eq!(out["restored"], json!(id), "{out}");
                }
            }
            // The Shots panel's Open and Save (ADR-8730): `editor.shot_panel_open` / `_save`.
            "shots.open" | "shots.save" => {
                let path = "assets/cine/parity.shots.ron";
                if !dir.join(path).is_file() {
                    agent.call(
                        "editor.create_asset",
                        json!({ "kind": "shot_list", "path": path }),
                    );
                }
                let opened = agent.call("editor.shot_panel_open", json!({ "path": path, "discard": true }));
                assert_eq!(opened["path"], path, "{opened}");
                if action.id == "shots.save" {
                    let saved = agent.call("editor.shot_panel_save", json!({}));
                    assert_eq!(saved["path"], path, "{saved}");
                    assert_eq!(saved["dirty"], false, "{saved}");
                }
            }
            // Content ▸ Import settings… and the panel's Apply / Revert (ADR-7469): open an imported
            // PNG, edit, revert, edit, apply (one cook).
            "content.import_settings"
            | "import_settings.apply"
            | "import_settings.revert"
            | "import_settings.field_reset" => {
                let source = dir.join("assets/parity_import.png");
                if !source.with_extension("png.meta").is_file() {
                    use image::ImageEncoder;
                    let mut bytes = Vec::new();
                    image::codecs::png::PngEncoder::new(&mut bytes)
                        .write_image(&[255u8, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
                        .expect("png encode");
                    let outside = dir.join("parity_import.png");
                    std::fs::write(&outside, bytes).expect("png");
                    regia_editor::content::import_files(
                        &dir,
                        "",
                        std::slice::from_ref(&outside),
                        Some("parity"),
                    )
                    .expect("import");
                }
                let state = agent.call(
                    "editor.import_settings_open",
                    json!({ "path": "assets/parity_import.png" }),
                );
                assert_eq!(state["importer"], "texture", "{state}");
                agent.call(
                    "editor.import_settings_edit",
                    json!({ "field": "max_size", "value": 64 }),
                );
                match action.id {
                    "import_settings.revert" => {
                        let out = agent.call("editor.import_settings_revert", json!({}));
                        assert_eq!(out["dirty"], false, "{out}");
                    }
                    "import_settings.field_reset" => {
                        // A field's Reset button is `editor.import_settings_edit { reset: true }` (ADR-7744).
                        let out = agent.call(
                            "editor.import_settings_edit",
                            json!({ "field": "max_size", "reset": true }),
                        );
                        let field = out["fields"]
                            .as_array()
                            .and_then(|rows| rows.iter().find(|row| row["key"] == "max_size"))
                            .expect("max_size row");
                        assert_eq!(field["set"], false, "{out}");
                    }
                    "import_settings.apply" => {
                        let out = agent.call("editor.import_settings_apply", json!({}));
                        assert!(out["result"]["cooked"].is_u64(), "{out}");
                    }
                    _ => {}
                }
            }
            // The asset viewer's mesh overlays (ADR-7469) draw through `editor.view_asset { overlay }`,
            // which needs a GPU device (`view_asset_overlays_through_the_dispatcher` renders them);
            // here: the method knows each overlay name and refuses an unknown one by code.
            "asset_viewer.overlay_uv"
            | "asset_viewer.overlay_uv1"
            | "asset_viewer.overlay_normals"
            | "asset_viewer.overlay_collision" => {
                let name = action.id.trim_start_matches("asset_viewer.overlay_");
                let overlay = regia_editor::asset_viewer::MeshOverlay::from_name(name)
                    .unwrap_or_else(|| panic!("{} names overlay {name}", action.id));
                assert_eq!(overlay.name(), name);
                let ActionPath::Method(method) = action.path else {
                    panic!("{}: a method path", action.id);
                };
                assert_eq!(method, "editor.view_asset");
            }
            // The Package for platform window (ADR-7475).
            "package.start" => {
                grant_build(&mut agent);
                let err = agent
                    .try_call("editor.package_start", json!({ "dry_run": true }))
                    .expect_err("no Regia.toml");
                assert_eq!(reason(&err), "package_no_project", "{err:?}");
            }
            "package.open_log" | "package.show_output" => {
                // Open a file in the OS viewer; `editor.package_status` answers the paths.
                assert_eq!(action.path, ActionPath::ViewOnly);
                let out = agent.call("editor.package_status", json!({}));
                assert_eq!(out["state"], "idle", "{out}");
            }
            "project_settings.icon_choose" => {
                assert_eq!(action.path, ActionPath::OsDialog("editor.project_icon"));
                let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
                png.extend_from_slice(b"IHDR");
                png.extend_from_slice(&[0, 0, 0, 8, 0, 0, 0, 8, 8, 6, 0, 0, 0]);
                std::fs::write(dir.join("assets/picked.png"), &png).expect("png");
                let out = agent.call("editor.project_icon", json!({ "path": "assets/picked.png" }));
                assert_eq!(out["set"], true, "{out}");
                assert_eq!(
                    std::fs::read(dir.join("assets/branding/icon.png")).ok(),
                    Some(png)
                );
            }
            // The confirm window's two buttons (rev-claude-63 finding G): Replace overwrites
            // through the same method, Keep is view-only.
            "project_settings.icon_replace" => {
                let ActionPath::Method(method) = action.path else {
                    panic!("{}: a method path", action.id);
                };
                assert_eq!(method, "editor.project_icon");
                let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
                png.extend_from_slice(b"IHDR");
                png.extend_from_slice(&[0, 0, 0, 12, 0, 0, 0, 12, 8, 6, 0, 0, 0]);
                std::fs::write(dir.join("assets/replacement.png"), &png).expect("png");
                let out = agent.call("editor.project_icon", json!({ "path": "assets/replacement.png" }));
                assert_eq!(out["set"], true, "{out}");
                assert_eq!(
                    std::fs::read(dir.join("assets/branding/icon.png")).ok(),
                    Some(png)
                );
            }
            "project_settings.icon_keep" => {
                assert_eq!(action.path, ActionPath::ViewOnly);
                let info = agent.call("editor.project_icon_info", json!({}));
                assert_eq!(info["set"], true, "{info}");
            }
            // The Inspector's per-component Remove (ADR-7472): the same edit as `editor.remove_component`.
            "inspector.remove_component" => {
                let entity = agent.call("editor.spawn", json!({ "name": "Lamp" }))["entity"]
                    .as_u64()
                    .expect("entity");
                agent.call(
                    "editor.add_component",
                    json!({ "entity": entity, "component": "PointLight" }),
                );
                let out = agent.call(
                    "editor.remove_component",
                    json!({ "entity": entity, "component": "PointLight" }),
                );
                assert!(out["undoable"].as_u64().is_some_and(|depth| depth > 0), "{out}");
                let err = agent
                    .try_call(
                        "editor.remove_component",
                        json!({ "entity": entity, "component": "PointLight" }),
                    )
                    .expect_err("already removed");
                assert_eq!(code(&err), "F0003", "{err:?}");
            }
            "abilities.asset_advanced" => {
                // The raw RON text view of the asset the typed form edits; agents read the file.
                assert_eq!(action.path, ActionPath::ViewOnly);
            }
            // The Abilities asset editor's Undo and Redo (ADR-7731): `editor.abilities_panel_asset
            // { undo | redo: true }` steps the same history; a freshly opened asset has nothing to step.
            "abilities.asset_undo" | "abilities.asset_redo" => {
                assert_eq!(action.path, ActionPath::Method("editor.abilities_panel_asset"));
                let rel = "abilities/parity_history.ability.ron";
                if !dir.join("assets").join(rel).is_file() {
                    agent.call(
                        "editor.create_asset",
                        json!({ "kind": "ability", "path": format!("assets/{rel}") }),
                    );
                }
                let (params, want) = if action.id == "abilities.asset_undo" {
                    (
                        json!({ "path": rel, "undo": true }),
                        "abilities_panel_nothing_to_undo",
                    )
                } else {
                    (
                        json!({ "path": rel, "redo": true }),
                        "abilities_panel_nothing_to_redo",
                    )
                };
                let err = agent
                    .try_call("editor.abilities_panel_asset", params)
                    .expect_err("nothing to step");
                assert_eq!(reason(&err), want, "{err:?}");
            }
            // The Output Log launch bar (ADR-8980, ADR-7548).
            "launch.standalone" | "launch.android" => {
                let ActionPath::Method(method) = action.path else {
                    panic!("{}: a method path", action.id);
                };
                assert_eq!(method, "editor.launch");
                grant_build(&mut agent);
                let project = agent
                    .world()
                    .remove_resource::<regia_editor::agent_ext::EditorProject>();
                let target = if action.id == "launch.android" {
                    "android"
                } else {
                    "standalone"
                };
                let err = agent
                    .try_call("editor.launch", json!({ "target": target }))
                    .expect_err("no project");
                assert_eq!(code(&err), "launch_no_project", "{err:?}");
                if let Some(project) = project {
                    agent.world().insert_resource(project);
                }
            }
            "tools.bake_captures_cancel" => {
                // The bake window's Cancel is `editor.cancel_task { id: "bake_captures" }`.
                assert_eq!(action.path, ActionPath::Method("editor.cancel_task"));
                let err = agent
                    .try_call("editor.cancel_task", json!({ "id": "bake_captures" }))
                    .expect_err("no bake runs");
                assert_eq!(code(&err), "no_such_task", "{err:?}");
            }
            "import_settings.apply_cancel" => {
                // The Import settings window's Cancel is `editor.cancel_job { id: "import_settings:<n>" }`
                // (ADR-7664); no write runs here, so an id the editor never started is refused.
                assert_eq!(action.path, ActionPath::Method("editor.cancel_job"));
                let err = agent
                    .try_call("editor.cancel_job", json!({ "id": "import_settings:999" }))
                    .expect_err("no write runs");
                assert_eq!(reason(&err), "job_not_found", "{err:?}");
            }
            // The launch bar's Device screen row (ADR-7836): no device and no Android launch.
            "launch.mirror_start" | "launch.mirror_stop" | "launch.mirror_tap" => {
                assert_eq!(action.path, ActionPath::Method("editor.device_mirror"));
                grant_build(&mut agent);
                match action.id {
                    "launch.mirror_start" => {
                        let err = agent
                            .try_call("editor.device_mirror", json!({ "action": "start" }))
                            .expect_err("no device");
                        assert_eq!(code(&err), "mirror_no_device", "{err:?}");
                    }
                    "launch.mirror_stop" => {
                        let out = agent.call("editor.device_mirror", json!({ "action": "stop" }));
                        assert_eq!(out["stopped"], false, "{out}");
                    }
                    _ => {
                        let err = agent
                            .try_call("editor.device_mirror", json!({ "action": "tap", "x": 1, "y": 1 }))
                            .expect_err("not running");
                        assert_eq!(code(&err), "mirror_not_running", "{err:?}");
                    }
                }
            }
            "launch.stop" => {
                grant_build(&mut agent);
                let out = agent.call("editor.launch_stop", json!({}));
                assert_eq!(out["stopped"], false, "{out}");
            }
            "launch.refresh_devices" => {
                // An adb that does not exist: the refusal path, and no real adb is ever started.
                let mut launcher = regia_editor::launch::Launcher::default();
                launcher.tools = regia_editor::launch::LaunchTools {
                    adb: Some(dir.join("no-such-adb")),
                    path_env: Some(std::ffi::OsString::new()),
                    sdk: regia_editor::launch::SdkSource::None,
                    ..regia_editor::launch::LaunchTools::default()
                };
                agent.world().insert_resource(launcher);
                let err = agent
                    .try_call("editor.launch_devices", json!({}))
                    .expect_err("no adb");
                assert_eq!(code(&err), "launch_no_adb", "{err:?}");
            }
            // The Layers panel's definition controls (ADR-7614): `editor.layer_create` writes the
            // file, `editor.layer_bind` / `editor.layer_initial` / `editor.layer_delete` answer.
            "layers.create_layer" | "layers.new_editor_only" => {
                let kind = if action.id == "layers.new_editor_only" {
                    "editor_only"
                } else {
                    "runtime"
                };
                let layer = action.id.replace('.', "_");
                let out = agent.call(
                    "editor.layer_create",
                    json!({ "defs": "layers/parity.layers.ron", "layer": layer, "kind": kind }),
                );
                assert_eq!(out["layer"], layer.as_str(), "{out}");
                assert!(dir.join("assets/layers/parity.layers.ron").is_file());
            }
            "layers.bind" | "layers.unbind" => {
                let err = agent
                    .try_call(
                        "editor.layer_bind",
                        json!({ "defs": "layers/none.layers.ron", "bind": action.id == "layers.bind" }),
                    )
                    .expect_err("no such definition file");
                assert_eq!(reason(&err), "layer_defs_not_found", "{err:?}");
            }
            "layers.initial" => {
                let err = agent
                    .try_call(
                        "editor.layer_initial",
                        json!({ "layer": "nowhere", "initial": "parked" }),
                    )
                    .expect_err("not a bound layer");
                assert!(
                    ["layer_unknown", "level_not_found"].contains(&reason(&err).as_str()),
                    "{err:?}"
                );
            }
            "layers.delete_layer" => {
                let err = agent
                    .try_call(
                        "editor.layer_delete",
                        json!({ "defs": "layers/none.layers.ron", "layer": "x" }),
                    )
                    .expect_err("no such definition file");
                assert_eq!(reason(&err), "layer_defs_not_found", "{err:?}");
            }
            "layers.refresh_defs" | "prefab_variant.create_cancel" => {
                assert_eq!(action.path, ActionPath::ViewOnly);
            }
            // Edit and Break of a placed level (spec 37 § 6.5): a plain entity is `not_placed`.
            "layers.edit_placed"
            | "outliner.edit_placed"
            | "layers.break_placed"
            | "outliner.break_placed" => {
                let entity = agent.call("editor.spawn", json!({ "name": "Not placed" }))["entity"]
                    .as_u64()
                    .expect("entity");
                let ActionPath::Method(method) = action.path else {
                    panic!("{}: a method path", action.id);
                };
                let params = if method == "editor.placed_edit" {
                    json!({ "action": "begin", "entity": entity })
                } else {
                    json!({ "entity": entity })
                };
                let err = agent.try_call(method, params).expect_err("not placed");
                assert_eq!(code(&err), "not_placed", "{err:?}");
            }
            // Create variant… and Clear override (spec 83 § 2.4): the variant methods.
            "outliner.create_variant" | "content.create_variant" | "prefab_variant.create_confirm" => {
                let err = agent
                    .try_call(
                        "editor.create_prefab_variant",
                        json!({ "base": "assets/none.prefab.ron", "path": "assets/v.variant.ron" }),
                    )
                    .expect_err("no base");
                assert_eq!(reason(&err), "base_missing", "{err:?}");
            }
            // The override marker's menu beside a field (ADR-7744): Revert and Apply are
            // `editor.prefab_override { action }`; a plain entity answers `not_prefab_instance`.
            "inspector.override_revert" | "inspector.override_apply" => {
                let entity = agent.call("editor.spawn", json!({ "name": "Plain marker" }))["entity"]
                    .as_u64()
                    .expect("entity");
                let verb = action.id.trim_start_matches("inspector.override_");
                let err = agent
                    .try_call(
                        "editor.prefab_override",
                        json!({ "entity": entity, "component": "Transform", "path": "translation", "action": verb }),
                    )
                    .expect_err("a plain entity is not a prefab instance");
                assert_eq!(code(&err), "not_prefab_instance", "{err:?}");
            }
            "inspector.clear_variant_override" => {
                let err = agent
                    .try_call(
                        "editor.set_variant_override",
                        json!({
                            "path": "assets/none.variant.ron",
                            "target": "8b6f2a11-0000-4000-8000-000000000002",
                            "component": "regia_scene::components::Tags",
                            "value": null,
                        }),
                    )
                    .expect_err("no variant");
                assert_eq!(reason(&err), "variant_missing", "{err:?}");
            }
            // The Shots panel's Edit player's shots (ADR-8940): `editor.shot_panel_adopt`. This
            // level has no sequence player with its own shot list, so the method answers its code.
            "shots.adopt" => {
                let err = agent
                    .try_call("editor.shot_panel_adopt", json!({ "discard": true }))
                    .expect_err("no player has a shot list");
                // The Shots panel's codes are `data.code` (book/agents/shot-panel.md).
                let code = err
                    .data
                    .as_ref()
                    .and_then(|d| d.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                assert_eq!(code, "shot_panel_no_player", "{err:?}");
            }
            // A Content Browser folder's menu (ADR-7725): the panel methods' `folder` param, and
            // `editor.asset_audit`'s. An inline set: one asset under `assets/props`.
            "content.folder_validate"
            | "content.folder_reference_graph"
            | "content.folder_size_map"
            | "content.folder_audit" => {
                const SET: &str = "(format_version: 2, assets: [(path: \"assets/props/a.mesh\", kind: mesh, cooked_bytes: 1)])";
                let (method, params) = match action.id {
                    "content.folder_validate" => (
                        "editor.problems_panel",
                        json!({ "set": SET, "run": true, "folder": "assets/props" }),
                    ),
                    "content.folder_reference_graph" => (
                        "editor.reference_graph_panel",
                        json!({ "set": SET, "action": "open", "folder": "assets/props" }),
                    ),
                    "content.folder_size_map" => (
                        "editor.size_map_panel",
                        json!({ "set": SET, "folder": "assets/props" }),
                    ),
                    _ => (
                        "editor.asset_audit",
                        json!({ "set": SET, "folder": "assets/props" }),
                    ),
                };
                agent.try_call(method, params).expect("the folder's assets");
                let err = agent
                    .try_call(
                        "editor.size_map_panel",
                        json!({ "set": SET, "folder": "assets/empty" }),
                    )
                    .expect_err("an empty folder");
                assert_eq!(code(&err), "asset_folder_empty", "{err:?}");
            }
            // The Render movie dialog (ADR-7726): Render is `editor.render_movie` (checked with the
            // Sequence menu item); Cancel is `editor.cancel_task { id: "render_movie" }`.
            "render_movie.start" => {
                let err = agent
                    .try_call(
                        "editor.render_movie",
                        json!({ "sequence": "assets/none.seq.ron" }),
                    )
                    .expect_err("no such sequence");
                assert_eq!(reason(&err), "render_movie_bad_sequence", "{err:?}");
            }
            "render_movie.cancel" => {
                assert_eq!(action.path, ActionPath::Method("editor.cancel_task"));
                let err = agent
                    .try_call("editor.cancel_task", json!({ "id": "render_movie" }))
                    .expect_err("no render runs");
                assert_eq!(code(&err), "no_such_task", "{err:?}");
            }
            // Restore on stop (ADR-7726): one undo entry on the panel's sequence.
            "seq.restore_on_stop" => {
                let path = "assets/cine/parity_restore.seq.ron";
                let player =
                    agent.call("editor.seq_new", json!({ "path": path, "length": 1.0 }))["player"].clone();
                let depth = agent.undo_depth();
                let out = agent.call(
                    "editor.seq_restore_on_stop",
                    json!({ "value": true, "player": player }),
                );
                assert_eq!(out["restore_on_stop"], true, "{out}");
                assert_eq!(agent.undo_depth(), depth + 1);
            }
            // The Shots panel's position and rotation keys (ADR-7726): `editor.shot_panel_edit`.
            "shots.pose_key_add" | "shots.pose_key_delete" => {
                let path = "assets/cine/parity_pose.shots.ron";
                if !dir.join(path).is_file() {
                    agent.call(
                        "editor.create_asset",
                        json!({ "kind": "shot_list", "path": path }),
                    );
                }
                agent.call("editor.shot_panel_open", json!({ "path": path, "discard": true }));
                let added = agent.call(
                    "editor.shot_panel_edit",
                    json!({ "track": "position", "op": "add", "time": 0.5, "value": [1.0, 2.0, 3.0] }),
                );
                assert_eq!(added["applied"], true, "{added}");
                if action.id == "shots.pose_key_delete" {
                    let index = added["selected"].clone();
                    let removed = agent.call(
                        "editor.shot_panel_edit",
                        json!({ "track": "position", "op": "remove", "index": index }),
                    );
                    assert_eq!(removed["keys"].as_array().map(Vec::len), Some(1), "{removed}");
                }
            }
            // The Inspector's Lock, pinned windows, Modified only, Pin to top and Add component
            // (ADR-7758): each control's agent method over the dispatcher, with its effect read back.
            "inspector.lock" => {
                assert_eq!(action.path, ActionPath::Method("editor.inspector_lock"));
                let entity = agent.call("editor.spawn", json!({ "name": "Locked" }))["entity"].clone();
                let on = agent.call("editor.inspector_lock", json!({ "on": true, "entity": entity }));
                assert_eq!(on["locked"], true, "{on}");
                assert_eq!(on["entity"], entity, "{on}");
                let off = agent.call("editor.inspector_lock", json!({ "on": false }));
                assert_eq!(off["locked"], false, "{off}");
                let err = agent
                    .try_call("editor.inspector_lock", json!({}))
                    .expect_err("no `on`");
                assert_eq!(reason(&err), "on_required", "{err:?}");
            }
            "inspector.open_pinned" => {
                assert_eq!(action.path, ActionPath::Method("editor.inspector_open_pinned"));
                let entity = agent.call("editor.spawn", json!({ "name": "Pinned" }))["entity"].clone();
                let open = agent.call("editor.inspector_open_pinned", json!({ "entity": entity }));
                assert_eq!(open["open"], true, "{open}");
                assert_eq!(open["opened"], true, "{open}");
                assert!(
                    open["windows"].as_array().is_some_and(|w| w.contains(&entity)),
                    "{open}"
                );
                let closed = agent.call(
                    "editor.inspector_open_pinned",
                    json!({ "entity": entity, "close": true }),
                );
                assert_eq!(closed["open"], false, "{closed}");
            }
            "inspector.modified_only" => {
                assert_eq!(action.path, ActionPath::Method("editor.inspect_selection"));
                let entity = agent.call("editor.spawn", json!({ "name": "Modified" }))["entity"].clone();
                let out = agent.call(
                    "editor.inspect_selection",
                    json!({ "entities": [entity], "modified_only": true }),
                );
                assert_eq!(out["filter"]["modified_only"], true, "{out}");
            }
            "inspector.pin_field" => {
                // No `EditorConfigPaths` in this world: the pin lives for the session, no file.
                assert_eq!(action.path, ActionPath::Method("editor.inspector_pin"));
                let on = agent.call(
                    "editor.inspector_pin",
                    json!({ "type": "PointLight", "path": "intensity", "on": true }),
                );
                assert_eq!(on["pinned"], true, "{on}");
                assert_eq!(on["changed"], true, "{on}");
                let off = agent.call(
                    "editor.inspector_pin",
                    json!({ "type": "PointLight", "path": "intensity", "on": false }),
                );
                assert_eq!(off["pinned"], false, "{off}");
            }
            "inspector.add_component" => {
                assert_eq!(action.path, ActionPath::Method("editor.add_component"));
                let entity = agent.call("editor.spawn", json!({ "name": "Added" }))["entity"].clone();
                let before = agent.undo_depth();
                let out = agent.call(
                    "editor.add_component",
                    json!({ "entity": entity, "component": "PointLight" }),
                );
                assert_eq!(out["component"], "PointLight", "{out}");
                assert_eq!(agent.undo_depth(), before + 1, "one undo step");
                agent
                    .try_call(
                        "editor.add_component",
                        json!({ "entity": entity, "component": "PointLight" }),
                    )
                    .expect_err("already has it");
            }
            // The Inspector's field and component tools (ADR-7773, w12-inspector-fields).
            "inspector.copy_value"
            | "inspector.copy_ron"
            | "inspector.copy_agent_path"
            | "inspector.paste_value" => {
                assert_eq!(
                    action.path,
                    ActionPath::Method(if action.id == "inspector.paste_value" {
                        "editor.property_paste"
                    } else {
                        "editor.property_copy"
                    })
                );
                let a = agent.call("editor.spawn", json!({ "name": "CopyA" }))["entity"].clone();
                let b = agent.call("editor.spawn", json!({ "name": "CopyB" }))["entity"].clone();
                for e in [&a, &b] {
                    agent.call(
                        "editor.add_component",
                        json!({ "entity": e, "component": "PointLight" }),
                    );
                }
                agent.call(
                    "editor.set_property",
                    json!({ "entity": a, "component": "PointLight", "path": "intensity", "value": 1234.0 }),
                );
                let copied = agent.call(
                    "editor.property_copy",
                    json!({ "entity": a, "component": "PointLight", "path": "intensity" }),
                );
                assert_eq!(copied["clipboard"]["ron"], "1234.0", "{copied}");
                assert_eq!(copied["agent_call"]["method"], "editor.set_property", "{copied}");
                let before = agent.undo_depth();
                agent.call(
                    "editor.property_paste",
                    json!({ "entity": b, "component": "PointLight", "path": "intensity" }),
                );
                assert_eq!(agent.undo_depth(), before + 1, "a paste is one undo step");
            }
            "inspector.reset_field" | "inspector.reset_component" => {
                assert_eq!(action.path, ActionPath::Method("editor.reset_property"));
                let e = agent.call("editor.spawn", json!({ "name": "Reset" }))["entity"].clone();
                agent.call(
                    "editor.add_component",
                    json!({ "entity": e, "component": "PointLight" }),
                );
                agent.call(
                    "editor.set_property",
                    json!({ "entity": e, "component": "PointLight", "path": "intensity", "value": 999.0 }),
                );
                let before = agent.undo_depth();
                let params = if action.id == "inspector.reset_field" {
                    json!({ "entity": e, "component": "PointLight", "path": "intensity" })
                } else {
                    json!({ "entity": e, "component": "PointLight", "all": true })
                };
                agent.call("editor.reset_property", params);
                assert_eq!(agent.undo_depth(), before + 1, "{}: one undo step", action.id);
            }
            "inspector.copy_component" | "inspector.paste_component" | "inspector.paste_new_component" => {
                let want = if action.id == "inspector.copy_component" {
                    "editor.component_copy"
                } else {
                    "editor.component_paste"
                };
                assert_eq!(action.path, ActionPath::Method(want));
                let a = agent.call("editor.spawn", json!({ "name": "CompA" }))["entity"].clone();
                let bare = agent.call("editor.spawn", json!({ "name": "CompBare" }))["entity"].clone();
                agent.call(
                    "editor.add_component",
                    json!({ "entity": a, "component": "PointLight" }),
                );
                agent.call(
                    "editor.component_copy",
                    json!({ "entity": a, "component": "PointLight" }),
                );
                let before = agent.undo_depth();
                agent.call(
                    "editor.component_paste",
                    json!({ "entity": bare, "as_new": true }),
                );
                assert_eq!(agent.undo_depth(), before + 1, "paste as new is one undo step");
                agent.call(
                    "editor.component_paste",
                    json!({ "entity": bare, "component": "PointLight" }),
                );
            }
            "inspector.raw_view" => {
                assert_eq!(action.path, ActionPath::Method("editor.inspect_selection"));
                let e = agent.call("editor.spawn", json!({ "name": "Raw" }))["entity"].clone();
                let out = agent.call(
                    "editor.inspect_selection",
                    json!({ "entities": [e], "raw": true }),
                );
                assert!(out.get("components").is_some(), "{out}");
            }
            "inspector.toggle_group" | "inspector.list_clear" => {
                assert_eq!(action.path, ActionPath::Method("editor.set_property"));
                let e = agent.call("editor.spawn", json!({ "name": "Toggle" }))["entity"].clone();
                agent.call(
                    "editor.add_component",
                    json!({ "entity": e, "component": "PointLight" }),
                );
                let before = agent.undo_depth();
                agent.call(
                    "editor.set_property",
                    json!({ "entity": e, "component": "PointLight", "path": "intensity", "value": 5.0 }),
                );
                assert_eq!(agent.undo_depth(), before + 1);
            }
            "inspector.list_duplicate" | "inspector.list_move" => {
                let method = if action.id == "inspector.list_move" {
                    "editor.list_move"
                } else {
                    "editor.list_duplicate"
                };
                assert_eq!(action.path, ActionPath::Method(method));
                let e = agent.call("editor.spawn", json!({ "name": "List" }))["entity"].clone();
                agent.call(
                    "editor.add_component",
                    json!({ "entity": e, "component": "PointLight" }),
                );
                // The route reaches the method: a field that is no list is refused by it.
                let params = json!({ "entity": e, "component": "PointLight", "path": "intensity", "index": 0, "from": 0, "to": 0 });
                agent
                    .try_call(method, params)
                    .expect_err("intensity is not a list");
            }
            // The status bar, the drawers and Quick settings (ADR-7754).
            "status.content_drawer" | "status.log_drawer" => {
                let ActionPath::Command(id) = action.path else {
                    panic!("{}: a palette command", action.id);
                };
                assert!(agent.command(id).ends_with("drawer open"), "{id}");
                assert!(agent.command(id).ends_with("drawer closed"), "{id}");
            }
            "status.problems" => {
                assert_eq!(agent.command("window.open_problems"), "Problems tab");
                assert!(
                    agent
                        .layout_requests()
                        .contains(&LayoutRequest::Open(Panel::Problems))
                );
            }
            "status.save_all" => {
                agent.call("editor.menu_invoke", json!({ "path": "File/Save all" }));
            }
            "status.job_cancel" | "status.trace" => {
                let (method, params) = if action.id == "status.trace" {
                    ("editor.trace_record", json!({ "action": "status" }))
                } else {
                    ("editor.cancel_task", json!({ "id": "package" }))
                };
                if let Err(err) = agent.try_call(method, params) {
                    assert_ne!(
                        err.code,
                        regia_agent::protocol::METHOD_NOT_FOUND,
                        "{method}: {err:?}"
                    );
                }
            }
            "drawer.dock" | "drawer.close" => {
                agent.call("editor.drawer", json!({ "drawer": "content", "open": true }));
                let params = if action.id == "drawer.dock" {
                    json!({ "drawer": "content", "dock": true })
                } else {
                    json!({ "drawer": "content", "open": false })
                };
                assert_eq!(agent.call("editor.drawer", params)["open_drawer"], Value::Null);
            }
            "quick.snap" => {
                assert_eq!(
                    agent.call("editor.set_snap", json!({ "enabled": true }))["enabled"],
                    true
                );
            }
            "quick.realtime" => {
                assert!(
                    agent
                        .command("view.toggle_realtime")
                        .starts_with("Realtime viewport")
                );
            }
            "quick.show_grid"
            | "quick.show_icons"
            | "quick.show_bounds"
            | "quick.show_colliders"
            | "quick.show_stats"
            | "quick.preview_sun" => {
                let flag = match action.id {
                    "quick.show_grid" => "grid",
                    "quick.show_icons" => "icons",
                    "quick.show_bounds" => "bounds",
                    "quick.show_colliders" => "colliders",
                    "quick.show_stats" => "stats",
                    _ => "preview_light",
                };
                agent.call("editor.set_viewport", json!({ flag: false }));
            }
            "quick.autosave" => {
                agent.call(
                    "editor.prefs_set",
                    json!({ "values": { "autosave.interval_minutes": 0 } }),
                );
            }
            // File ▸ Screenshot…'s dialog (ADR-7768): the capture is `editor.screenshot`; Open folder
            // only touches the OS file manager; Dismiss (and the window's close) is
            // `editor.screenshot { close: true }` (ADR-8583).
            "screenshot.by_multiplier" | "screenshot.helpers" | "screenshot.take" => {
                assert_eq!(action.path, ActionPath::Method("editor.screenshot"));
                let err = agent
                    .try_call("editor.screenshot", json!({ "multiplier": 0 }))
                    .expect_err("a zero multiplier is refused");
                assert!(
                    reason(&err) == "screenshot_bad_multiplier" || code(&err) == "screenshot_bad_multiplier",
                    "{err:?}"
                );
            }
            "screenshot.open_folder" => {
                assert_eq!(action.path, ActionPath::ViewOnly);
            }
            "screenshot.dismiss" => {
                assert_eq!(action.path, ActionPath::Method("editor.screenshot"));
                agent.call("editor.command", json!({ "run": "file.screenshot" }));
                let out = agent.call("editor.screenshot", json!({ "close": true }));
                assert_eq!(out["closed"], true, "{out}");
            }
            // Edit in grid (ADR-7787): `editor.bulk_edit_open` with the rows; a missing asset is
            // refused with the data asset code before any grid opens.
            "outliner.edit_in_grid" | "content.edit_in_grid" => {
                let err = agent
                    .try_call(
                        "editor.bulk_edit_open",
                        json!({ "assets": ["assets/none.data.ron"] }),
                    )
                    .expect_err("no such asset");
                assert_eq!(reason(&err), "data_asset_not_found", "{err:?}");
                let out = agent.call("editor.spawn", json!({ "name": "Grid row" }));
                let entity = out["entity"].as_u64().expect("entity");
                let out = agent.call("editor.bulk_edit_open", json!({ "entities": [entity] }));
                assert_eq!(out["rows_total"], 1, "{out}");
            }
            // The Prefab overrides window (ADR-7790): a plain entity is `not_prefab_instance`;
            // Overrides… in the Inspector and the Outliner open the window for an instance.
            "prefab_overrides.apply"
            | "prefab_overrides.apply_all"
            | "prefab_overrides.revert"
            | "prefab_overrides.revert_all"
            | "prefab_overrides.edit_in_place" => {
                let ActionPath::Method(method) = action.path else {
                    panic!("{}: a method path", action.id);
                };
                let entity = agent.call("editor.spawn", json!({ "name": "Not an instance" }))["entity"]
                    .as_u64()
                    .expect("entity");
                let err = agent
                    .try_call(method, json!({ "entity": entity }))
                    .expect_err("not an instance");
                assert_eq!(reason(&err), "not_prefab_instance", "{}: {err:?}", action.id);
            }
            "prefab_overrides.close" => {
                assert_eq!(action.path, ActionPath::ViewOnly);
            }
            "inspector.open_overrides" | "outliner.open_overrides" => {
                assert_eq!(action.path, ActionPath::Method("editor.prefab_overrides_open"));
                let source = agent.call("editor.spawn", json!({ "name": "Overrides source" }))["entity"]
                    .as_u64()
                    .expect("entity");
                let path = format!("assets/{}.prefab.ron", action.id.replace('.', "_"));
                let instance = agent.call(
                    "editor.create_prefab",
                    json!({ "entities": [source], "path": path }),
                )["instance"]
                    .as_u64()
                    .expect("instance");
                let out = agent.call("editor.prefab_overrides_open", json!({ "entity": instance }));
                assert_eq!(out["open"], true, "{out}");
                assert_eq!(out["rows"], 0, "a fresh instance has no overrides: {out}");
            }
            // Diff against HEAD… (ADR-7799): `editor.asset_diff`. The temp project is outside any
            // git work tree, so the method answers its code.
            "content.diff_head" | "graph.diff_head" => {
                std::fs::write(dir.join("assets/parity_diff.ron"), "(a: 1)").expect("write");
                let err = agent
                    .try_call("editor.asset_diff", json!({ "path": "assets/parity_diff.ron" }))
                    .expect_err("no revision control");
                assert!(
                    matches!(reason(&err).as_str(), "no_revision_control" | "not_in_revision"),
                    "{err:?}"
                );
            }
            // Diff against revision… and the window's Pick another revision…: the picker lists
            // `editor.asset_revisions`.
            "content.diff_revision" | "graph.diff_revision" | "asset_diff.pick_another" => {
                match agent.try_call(
                    "editor.asset_revisions",
                    json!({ "path": "assets/parity_diff.ron" }),
                ) {
                    Ok(out) => assert!(out["revisions"].is_array(), "{out}"),
                    Err(err) => assert_eq!(reason(&err), "no_revision_control", "{err:?}"),
                }
            }
            // The diff window's view controls (pan, zoom, the tree view's filter).
            "asset_diff.zoom_in" | "asset_diff.zoom_out" | "asset_diff.fit" | "asset_diff.show_unchanged" => {
                assert_eq!(action.path, ActionPath::ViewOnly);
            }
            // The Content Browser's selection actions and their dialogs (ADR-7822).
            "content.bulk_validate"
            | "content.bulk_reimport"
            | "content.bulk_save"
            | "content.bulk_move"
            | "content.bulk_move_confirm"
            | "content.bulk_delete"
            | "content.bulk_delete_confirm"
            | "content.bulk_add_to_collection" => {
                let name = match action.id {
                    "content.bulk_validate" => "validate",
                    "content.bulk_reimport" => "reimport",
                    "content.bulk_save" => "save",
                    "content.bulk_move" | "content.bulk_move_confirm" => "move",
                    "content.bulk_delete" | "content.bulk_delete_confirm" => "delete",
                    _ => "add_to_collection",
                };
                let err = agent
                    .try_call(
                        "editor.bulk_asset_action",
                        json!({ "paths": [], "action": name, "to": "assets/x", "collection": "x" }),
                    )
                    .expect_err("nothing selected");
                assert_eq!(reason(&err), "bulk_empty", "{err:?}");
            }
            "content.delete_replace" => {
                std::fs::write(dir.join("assets/parity_old.png"), b"old").expect("old");
                std::fs::write(dir.join("assets/parity_new.png"), b"new").expect("new");
                std::fs::write(
                    dir.join("assets/parity_ref.ron"),
                    "(t: \"assets/parity_old.png\")",
                )
                .expect("ref");
                let out = agent.call(
                    "editor.delete_asset",
                    json!({ "path": "assets/parity_old.png", "replace_with": "assets/parity_new.png", "confirm": true }),
                );
                assert_eq!(out["deleted"], true, "{out}");
                assert_eq!(
                    std::fs::read_to_string(dir.join("assets/parity_ref.ron")).expect("ref"),
                    "(t: \"assets/parity_new.png\")"
                );
            }
            "asset_panels.retarget" | "retarget.apply" => {
                std::fs::write(dir.join("assets/parity_target.png"), b"t").expect("target");
                let err = agent
                    .try_call(
                        "editor.retarget_references",
                        json!({ "from": "assets/parity_gone.png", "to": "assets/parity_target.png" }),
                    )
                    .expect_err("nothing references it");
                assert_eq!(reason(&err), "retarget_nothing", "{err:?}");
            }
            "asset_panels.find_similar" => {
                let out = agent.call(
                    "editor.retarget_references",
                    json!({ "from": "assets/gone/parity_target.png", "similar": true }),
                );
                assert!(out["similar"].is_array(), "{out}");
            }
            // The selection's Edit selection in grid (`editor.bulk_edit`, P-145, ADR-8147): a
            // selection with no data asset is refused with its code; data assets open the grid.
            "content.bulk_edit_in_grid" => {
                std::fs::write(dir.join("assets/parity_notes.txt"), b"notes").expect("notes");
                let err = agent
                    .try_call(
                        "editor.bulk_edit",
                        json!({ "assets": ["assets/parity_notes.txt"] }),
                    )
                    .expect_err("no data asset");
                assert_eq!(reason(&err), "bulk_edit_no_rows", "{err:?}");
            }
            // The selection's Export selection with dependencies… (`editor.export_package { paths }`,
            // P-145): every selected asset and its dependencies in one package.
            "content.bulk_export" => {
                std::fs::write(dir.join("assets/parity_a.ron"), b"(t: \"assets/parity_b.ron\")").expect("a");
                std::fs::write(dir.join("assets/parity_b.ron"), b"()").expect("b");
                std::fs::write(dir.join("assets/parity_c.ron"), b"()").expect("c");
                let dest = regia_test_utils::unique_temp_dir("agent-parity-shell-pkg").join("pkg");
                let out = agent.call(
                    "editor.export_package",
                    json!({ "paths": ["assets/parity_a.ron", "assets/parity_c.ron"], "dest": dest.display().to_string() }),
                );
                assert_eq!(
                    out["roots"],
                    json!(["assets/parity_a.ron", "assets/parity_c.ron"]),
                    "{out}"
                );
                assert_eq!(out["assets"].as_array().map(Vec::len), Some(3), "{out}");
            }
            "save_all.save_selected" | "save_all.dont_save" => {
                // Save selected is `editor.save_all { ids }`; Don't save passes no ids.
                let out = agent.call("editor.save_all", json!({ "ids": [] }));
                assert!(out["documents"].is_array(), "{out}");
            }
            // The confirmations' and the picker's Cancel close them (no state).
            "content.action_cancel" | "retarget.cancel" => {}
            // Save all's Cancel is `editor.save_all { cancel: true }` (ADR-8583).
            "save_all.cancel" => {
                assert_eq!(action.path, ActionPath::Method("editor.save_all"));
                agent.call("editor.save_all", json!({ "dialog": true }));
                let out = agent.call("editor.save_all", json!({ "cancel": true }));
                assert_eq!(out["cancelled"], true, "{out}");
            }
            // The source switch's Live link (ADR-7833): with no play session and no runtime selected
            // it refuses with its code; off is always accepted.
            "remote.live_link" => {
                let err = agent
                    .try_call("editor.live_link", json!({ "on": true }))
                    .expect_err("no session");
                assert_eq!(code(&err), "live_link_no_session", "{err:?}");
                let out = agent.call("editor.live_link", json!({ "on": false }));
                assert_eq!(out["on"], false, "{out}");
            }
            // The remote Outliner's More (ADR-7833): `editor.remote_select { pages }`; the edit world
            // is the source here, so it answers the view without a runtime.
            "remote.more" => {
                let out = agent.call("editor.remote_select", json!({ "pages": 2 }));
                assert_eq!(out["source"]["id"], "edit", "{out}");
                assert_eq!(out["pages"], 2, "{out}");
            }
            // The remote Outliner's Show or hide children: `editor.remote_select { collapse, expand }`;
            // the collapsed rows belong to the current source (the edit world here).
            "remote.toggle_children" => {
                let out = agent.call("editor.remote_select", json!({ "collapse": [7] }));
                assert_eq!(out["collapsed"], json!([7]), "{out}");
                let out = agent.call("editor.remote_select", json!({ "expand": [7] }));
                assert_eq!(out["collapsed"], json!([]), "{out}");
            }
            // Inspector ▸ Revert entity to base / Revert to base (ADR-7823): the open level is not a
            // level variant, so the method answers `no_variant_open`.
            "inspector.revert_level_variant_entity" | "inspector.revert_level_variant_field" => {
                assert_eq!(action.path, ActionPath::Method("editor.level_variant_revert"));
                let err = agent
                    .try_call(
                        "editor.level_variant_revert",
                        json!({ "entity": "8b6f2a11-0000-4000-8000-000000000002" }),
                    )
                    .expect_err("no variant open");
                assert_eq!(reason(&err), "no_variant_open", "{err:?}");
            }
            // Platforms ▸ Re-check (3261): the check runs `regia doctor` in a worker; headless the
            // method's refusals are checked (an unknown platform, an unknown op), not a CLI run.
            "platforms.recheck" => {
                assert_eq!(action.path, ActionPath::Method("editor.platform_check"));
                let err = agent
                    .try_call(
                        "editor.platform_check",
                        json!({ "op": "recheck", "platform": "plan9" }),
                    )
                    .expect_err("an unknown platform");
                assert_eq!(code(&err), "platform_bad_target", "{err:?}");
                let err = agent
                    .try_call("editor.platform_check", json!({ "op": "bogus" }))
                    .expect_err("an unknown op");
                assert_eq!(code(&err), "platform_bad_op", "{err:?}");
            }
            // Size Report ▸ Refresh report (3261): with no package reported there is nothing to
            // refresh.
            "size_report.refresh" => {
                assert_eq!(action.path, ActionPath::Method("editor.package_size_report"));
                let err = agent
                    .try_call("editor.package_size_report", json!({ "op": "refresh" }))
                    .expect_err("no package reported");
                assert_eq!(code(&err), "package_size_no_package", "{err:?}");
            }
            // Launch Profiles (3261): New only resets the dialog's draft.
            "launch_profiles.new" => assert_eq!(action.path, ActionPath::ViewOnly),
            // Save, Package before launch, Add/Remove argument and Delete all write
            // `config/launch_profiles.ron` through `editor.launch_profiles { op }`.
            "launch_profiles.save"
            | "launch_profiles.package"
            | "launch_profiles.add_arg"
            | "launch_profiles.remove_arg"
            | "launch_profiles.delete" => {
                assert_eq!(action.path, ActionPath::Method("editor.launch_profiles"));
                let name = format!("Parity {}", action.id);
                let mut profile = json!({
                    "name": name,
                    "target": "standalone",
                    "build_profile": "dev",
                    "cook": "incremental",
                    "package": action.id == "launch_profiles.package",
                    "device": null,
                    "start_level": null,
                    "extra_arguments": [],
                });
                if action.id == "launch_profiles.add_arg" || action.id == "launch_profiles.remove_arg" {
                    profile["extra_arguments"] = json!(["--parity", "1"]);
                }
                let out = agent.call(
                    "editor.launch_profiles",
                    json!({ "op": "save", "profile": profile }),
                );
                let saved = |out: &Value| -> Option<Value> {
                    out["profiles"]
                        .as_array()
                        .and_then(|p| p.iter().find(|p| p["name"] == name.as_str()).cloned())
                };
                let row = saved(&out).unwrap_or_else(|| panic!("{} saved: {out}", action.id));
                assert_eq!(row["package"], action.id == "launch_profiles.package", "{out}");
                assert!(dir.join("config/launch_profiles.ron").is_file());
                match action.id {
                    "launch_profiles.add_arg" => {
                        assert_eq!(row["extra_arguments"], json!(["--parity", "1"]), "{out}");
                    }
                    "launch_profiles.remove_arg" => {
                        profile["extra_arguments"] = json!(["--parity"]);
                        let out = agent.call(
                            "editor.launch_profiles",
                            json!({ "op": "save", "profile": profile }),
                        );
                        let row = saved(&out).expect("still saved");
                        assert_eq!(row["extra_arguments"], json!(["--parity"]), "{out}");
                    }
                    "launch_profiles.delete" => {
                        let out =
                            agent.call("editor.launch_profiles", json!({ "op": "delete", "name": name }));
                        assert!(saved(&out).is_none(), "{out}");
                        let err = agent
                            .try_call("editor.launch_profiles", json!({ "op": "delete", "name": name }))
                            .expect_err("deleted already");
                        assert_eq!(code(&err), "launch_profile_not_found", "{err:?}");
                    }
                    _ => {}
                }
            }
            // The Theme panel (3245): each action edits a fresh draft (`editor.theme_panel_open
            // { new }`) through `editor.theme_panel_set`, or `editor.theme_panel_import`.
            "theme.pin" => assert_eq!(action.path, ActionPath::ViewOnly),
            "theme.variation"
            | "theme.property"
            | "theme.reset"
            | "theme.all_states"
            | "theme.import"
            | "theme.undo"
            | "theme.redo"
            | "theme.preview_document"
            | "theme.type" => {
                let method = if action.id == "theme.import" {
                    "editor.theme_panel_import"
                } else {
                    "editor.theme_panel_set"
                };
                assert_eq!(action.path, ActionPath::Method(method));
                let theme = format!("parity_{}", action.id.replace('.', "_"));
                agent.call(
                    "editor.theme_panel_open",
                    json!({ "new": { "name": theme, "extends": "regia-dark" } }),
                );
                let variation = json!({
                    "variations": { "Button.parity": { "extends": "Button", "props": { "background": "#ca1234" } } }
                });
                match action.id {
                    "theme.variation" => {
                        let out = agent.call("editor.theme_panel_set", variation);
                        assert_eq!(out["changed"], json!(["variations"]), "{out}");
                    }
                    "theme.type" | "theme.property" | "theme.reset" => {
                        let out = agent.call(
                            "editor.theme_panel_set",
                            json!({ "types": { "Button": { "props": { "background": "#123456", "radius": 4 } } } }),
                        );
                        assert_eq!(out["changed"], json!(["types"]), "{out}");
                        if action.id == "theme.reset" {
                            // A `null` property removes the override.
                            let out = agent.call(
                                "editor.theme_panel_set",
                                json!({ "types": { "Button": { "props": { "radius": null } } } }),
                            );
                            assert_eq!(out["changed"], json!(["types"]), "{out}");
                        }
                    }
                    "theme.all_states" => {
                        let out = agent.call(
                            "editor.theme_panel_set",
                            json!({ "apply_all_states": { "type": "Button", "state": "normal" } }),
                        );
                        assert_eq!(out["changed"], json!(["apply_all_states"]), "{out}");
                    }
                    "theme.import" => {
                        let out = agent.call(
                            "editor.theme_panel_import",
                            json!({ "from": "regia-light", "items": [{ "type": "Button", "state": "normal", "kind": "colors" }] }),
                        );
                        assert!(
                            out["copied"]
                                .as_array()
                                .is_some_and(|c| c.contains(&json!("types.Button.props.background"))),
                            "{out}"
                        );
                    }
                    "theme.undo" | "theme.redo" => {
                        agent.call("editor.theme_panel_set", variation);
                        let out = agent.call("editor.theme_panel_set", json!({ "history": "undo" }));
                        assert_eq!(out["changed"], json!(["history"]), "{out}");
                        if action.id == "theme.redo" {
                            let out = agent.call("editor.theme_panel_set", json!({ "history": "redo" }));
                            assert_eq!(out["changed"], json!(["history"]), "{out}");
                        } else {
                            let err = agent
                                .try_call("editor.theme_panel_set", json!({ "history": "undo" }))
                                .expect_err("one step only");
                            assert_eq!(code(&err), "theme_panel_empty_history", "{err:?}");
                        }
                    }
                    _ => {
                        // Add preview document: a project UI document joins the gallery's pages.
                        let document = dir.join("assets/parity_theme.ui.ron");
                        let tmp = dir.join("assets/parity_theme.ui.ron.tmp");
                        std::fs::write(
                            &tmp,
                            r#"(version: 11, root: (children: [(widget: button, style: (size: (160, 48)))]))"#,
                        )
                        .expect("write document");
                        std::fs::rename(&tmp, &document).expect("publish document");
                        let out = agent.call(
                            "editor.theme_panel_set",
                            json!({ "preview_document": document.display().to_string() }),
                        );
                        assert!(
                            out["preview"]["page"]
                                .as_str()
                                .is_some_and(|p| p.ends_with("parity_theme.ui.ron")),
                            "{out}"
                        );
                    }
                }
            }
            // Undo History ▸ a row (3255): `editor.history_jump { index }` runs each undo or redo
            // to that boundary; past the end is refused.
            "history.jump" => {
                assert_eq!(action.path, ActionPath::Method("editor.history_jump"));
                agent.call("editor.spawn", json!({ "name": "History one" }));
                agent.call("editor.spawn", json!({ "name": "History two" }));
                let state = agent.call("editor.history", json!({}));
                let index = state["index"].as_u64().expect("index");
                assert!(index >= 2, "{state}");
                let out = agent.call("editor.history_jump", json!({ "index": index - 2 }));
                assert_eq!(out["index"], index - 2, "{out}");
                assert!(!agent.names().iter().any(|n| n == "History two"));
                let out = agent.call("editor.history_jump", json!({ "index": index }));
                assert_eq!(out["index"], index, "{out}");
                assert!(agent.names().iter().any(|n| n == "History two"));
                assert!(
                    agent
                        .try_call("editor.history_jump", json!({ "index": index + 100 }))
                        .is_err(),
                    "past the newest entry"
                );
            }
            // Notes (3259): Add places a dedicated marker; Reply, Resolve and Reopen edit its thread
            // and state; Frame focuses the camera on it.
            "notes.add" | "notes.reply" | "notes.resolve" | "notes.reopen" | "notes.frame" => {
                let method = match action.id {
                    "notes.add" => "editor.note_add",
                    "notes.reply" => "editor.note_reply",
                    "notes.frame" => "editor.focus",
                    _ => "editor.note_resolve",
                };
                assert_eq!(action.path, ActionPath::Method(method));
                let text = format!("Parity {}", action.id);
                let note = agent.call(
                    "editor.note_add",
                    json!({ "text": text, "position": [17.0, 0.0, -9.0] }),
                );
                let entity = note["entity"].as_u64().expect("note marker");
                let row = |agent: &mut Agent, filter: &str| -> Option<Value> {
                    agent.call("editor.note_list", json!({ "filter": filter }))["notes"]
                        .as_array()
                        .and_then(|rows| rows.iter().find(|r| r["entity"] == entity).cloned())
                };
                assert_eq!(
                    row(&mut agent, "open").expect("listed open")["note"]["text"],
                    text.as_str()
                );
                match action.id {
                    "notes.reply" => {
                        agent.call(
                            "editor.note_reply",
                            json!({ "entity": entity, "text": "Adjusted" }),
                        );
                        let replied = row(&mut agent, "open").expect("still open");
                        let thread = &replied["note"]["thread"];
                        assert_eq!(thread[0]["text"], "Adjusted", "{thread}");
                        assert!(
                            agent
                                .try_call("editor.note_reply", json!({ "entity": entity, "text": "" }))
                                .is_err(),
                            "an empty reply is refused"
                        );
                    }
                    "notes.resolve" | "notes.reopen" => {
                        agent.call(
                            "editor.note_resolve",
                            json!({ "entity": entity, "resolved": true }),
                        );
                        assert!(row(&mut agent, "open").is_none());
                        assert!(row(&mut agent, "resolved").is_some());
                        if action.id == "notes.reopen" {
                            agent.call(
                                "editor.note_resolve",
                                json!({ "entity": entity, "resolved": false }),
                            );
                            assert!(row(&mut agent, "resolved").is_none());
                            assert!(row(&mut agent, "open").is_some());
                        }
                    }
                    "notes.frame" => {
                        let before = agent.world().resource::<EditorCamera>().target;
                        let out = agent.call("editor.focus", json!({ "entities": [entity] }));
                        assert!(out["target"].is_array(), "{out}");
                        assert_ne!(
                            agent.world().resource::<EditorCamera>().target,
                            before,
                            "the camera moved to the note"
                        );
                    }
                    _ => {}
                }
            }
            // Activity ▸ Select entity (3259): `editor.select` selects the row's entity.
            "activity.select" => {
                assert_eq!(action.path, ActionPath::Method("editor.select"));
                let entity = agent.call("editor.spawn", json!({ "name": "Activity row" }))["entity"]
                    .as_u64()
                    .expect("entity");
                let timeline = agent.call("editor.activity", json!({ "since": 0 }));
                assert!(
                    timeline["entries"]
                        .as_array()
                        .is_some_and(|e| e.iter().any(|row| row["method"] == "editor.spawn")),
                    "{timeline}"
                );
                agent.call("editor.select", json!({ "entities": [entity] }));
                assert!(agent.world().resource::<Selection>().primary().is_some());
                agent.call("editor.select", json!({ "entities": [] }));
                assert!(agent.world().resource::<Selection>().is_empty());
            }
            // Terrain ▸ Region (3236): the mode toggle is view state; Copy, Paste, Export, Import
            // and the RAW byte order run `editor.terrain_region` on a small terrain.
            "terrain.region_mode" => assert_eq!(action.path, ActionPath::ViewOnly),
            "terrain.region_copy"
            | "terrain.region_paste"
            | "terrain.region_export"
            | "terrain.region_import"
            | "terrain.region_endian" => {
                assert_eq!(action.path, ActionPath::Method("editor.terrain_region"));
                allow_heavy(&mut agent);
                let terrain = agent.world().spawn(small_terrain()).id();
                let bits = terrain.to_bits();
                let copy = agent.call(
                    "editor.terrain_region",
                    json!({ "op": "copy", "entity": bits, "rect": [0, 0, 3, 2] }),
                );
                assert_eq!(copy["width"], 3, "{copy}");
                match action.id {
                    "terrain.region_paste" => {
                        let paste = agent.call(
                            "editor.terrain_region",
                            json!({ "op": "paste", "entity": bits, "at": [4, 4], "rotation": 90, "height_offset": 2.0 }),
                        );
                        assert_eq!(paste["changed"], 6, "{paste}");
                        let err = agent
                            .try_call(
                                "editor.terrain_region",
                                json!({ "op": "paste", "entity": bits, "rotation": 45 }),
                            )
                            .expect_err("a rotation off the quarter turns");
                        assert_eq!(code(&err), "terrain_region_invalid", "{err:?}");
                    }
                    "terrain.region_export" | "terrain.region_import" | "terrain.region_endian" => {
                        let (extension, endian) = if action.id == "terrain.region_endian" {
                            ("r16", "big")
                        } else {
                            ("png", "little")
                        };
                        let path = format!("assets/{}.{extension}", action.id.replace('.', "_"));
                        let exported = agent.call(
                            "editor.terrain_region",
                            json!({ "op": "export", "entity": bits, "rect": [0, 0, 3, 2], "path": path, "endian": endian }),
                        );
                        assert_eq!(exported["width"], 3, "{exported}");
                        assert!(dir.join(&path).is_file(), "{path}");
                        if action.id != "terrain.region_export" {
                            let imported = agent.call(
                                "editor.terrain_region",
                                json!({ "op": "import", "entity": bits, "rect": [4, 4, 3, 2], "path": path, "endian": endian }),
                            );
                            assert_eq!(imported["changed"], 6, "{imported}");
                        }
                        if action.id == "terrain.region_endian" {
                            let err = agent
                                .try_call(
                                    "editor.terrain_region",
                                    json!({ "op": "export", "entity": bits, "rect": [0, 0, 3, 2], "path": path, "endian": "middle" }),
                                )
                                .expect_err("an unknown byte order");
                            assert_eq!(code(&err), "terrain_region_invalid", "{err:?}");
                        }
                    }
                    _ => {}
                }
                let _ = agent.world().despawn(terrain);
            }
            // Terrain ▸ Layers (3237): add, set-height, base, select, hide, lock, move, delete,
            // rename and flatten run `terrain.edit_layer_*` on a small terrain's stack.
            "terrain.edit_layer_add"
            | "terrain.edit_layer_set_height"
            | "terrain.edit_layer_base"
            | "terrain.edit_layer_select"
            | "terrain.edit_layer_visible"
            | "terrain.edit_layer_lock"
            | "terrain.edit_layer_up"
            | "terrain.edit_layer_down"
            | "terrain.edit_layer_delete"
            | "terrain.edit_layer_rename"
            | "terrain.edit_layer_flatten" => {
                let method = match action.id {
                    "terrain.edit_layer_add" | "terrain.edit_layer_set_height" => "terrain.edit_layer_create",
                    "terrain.edit_layer_up" | "terrain.edit_layer_down" => "terrain.edit_layer_reorder",
                    "terrain.edit_layer_delete" => "terrain.edit_layer_delete",
                    "terrain.edit_layer_flatten" => "terrain.edit_layer_flatten",
                    _ => "terrain.edit_layer_set",
                };
                assert_eq!(action.path, ActionPath::Method(method));
                let terrain = agent.world().spawn(small_terrain()).id();
                let bits = terrain.to_bits();
                let blend = if action.id == "terrain.edit_layer_set_height" {
                    "set"
                } else {
                    "add"
                };
                let a = agent.call(
                    "terrain.edit_layer_create",
                    json!({ "entity": bits, "name": "A", "blend": blend }),
                );
                let a_id = a["layer"].as_u64().expect("layer id");
                assert_eq!(a["active"], a_id, "a new layer is made active: {a}");
                assert_eq!(a["layers"][0]["blend"], blend, "{a}");
                let b = agent.call(
                    "terrain.edit_layer_create",
                    json!({ "entity": bits, "name": "B" }),
                );
                assert_eq!(b["layers"][1]["name"], "B", "{b}");
                match action.id {
                    "terrain.edit_layer_base" => {
                        let out = agent.call(
                            "terrain.edit_layer_set",
                            json!({ "entity": bits, "layer": "base", "active": true }),
                        );
                        assert!(out["active"].is_null(), "the base is active: {out}");
                    }
                    "terrain.edit_layer_select" => {
                        let out = agent.call(
                            "terrain.edit_layer_set",
                            json!({ "entity": bits, "layer": "A", "active": true }),
                        );
                        assert_eq!(out["active"], a_id, "{out}");
                    }
                    "terrain.edit_layer_visible" => {
                        let out = agent.call(
                            "terrain.edit_layer_set",
                            json!({ "entity": bits, "layer": "A", "enabled": false }),
                        );
                        assert_eq!(out["layers"][0]["enabled"], false, "{out}");
                    }
                    "terrain.edit_layer_lock" => {
                        let out = agent.call(
                            "terrain.edit_layer_set",
                            json!({ "entity": bits, "layer": "A", "locked": true }),
                        );
                        assert_eq!(out["layers"][0]["locked"], true, "{out}");
                        let err = agent
                            .try_call(
                                "terrain.edit_layer_delete",
                                json!({ "entity": bits, "layer": "A" }),
                            )
                            .expect_err("a locked layer stays");
                        assert_eq!(code(&err), "edit_layer_locked", "{err:?}");
                    }
                    "terrain.edit_layer_up" => {
                        let out = agent.call(
                            "terrain.edit_layer_reorder",
                            json!({ "entity": bits, "layer": "A", "index": 1 }),
                        );
                        assert_eq!(out["layers"][1]["name"], "A", "{out}");
                    }
                    "terrain.edit_layer_down" => {
                        let out = agent.call(
                            "terrain.edit_layer_reorder",
                            json!({ "entity": bits, "layer": "B", "index": 0 }),
                        );
                        assert_eq!(out["layers"][0]["name"], "B", "{out}");
                    }
                    "terrain.edit_layer_delete" => {
                        let out = agent.call(
                            "terrain.edit_layer_delete",
                            json!({ "entity": bits, "layer": "A" }),
                        );
                        assert_eq!(out["layers"].as_array().map(Vec::len), Some(1), "{out}");
                        assert_eq!(out["layers"][0]["name"], "B", "{out}");
                    }
                    "terrain.edit_layer_rename" => {
                        let out = agent.call(
                            "terrain.edit_layer_set",
                            json!({ "entity": bits, "layer": "A", "name": "Renamed" }),
                        );
                        assert_eq!(out["layers"][0]["name"], "Renamed", "{out}");
                        let err = agent
                            .try_call(
                                "terrain.edit_layer_set",
                                json!({ "entity": bits, "layer": "Renamed", "opacity": 2.0 }),
                            )
                            .expect_err("opacity is 0..=1");
                        // The method's schema declares `opacity` as a 0..=1 range, so the protocol's limit check
                        // refuses it (universal F0002, naming the param) before the handler's own
                        // `edit_layer_bad_opacity`, which regia_agent's terrain_edit_layers unit test covers.
                        assert_eq!(code(&err), "F0002", "{err:?}");
                        let param = err.data.as_ref().and_then(|d| d["limit"]["param"].as_str());
                        assert_eq!(param, Some("opacity"), "{err:?}");
                    }
                    "terrain.edit_layer_flatten" => {
                        let out = agent.call("terrain.edit_layer_flatten", json!({ "entity": bits }));
                        assert_eq!(out["merged"], 2, "{out}");
                    }
                    _ => {}
                }
                let _ = agent.world().despawn(terrain);
            }
            // Terrain ▸ Layers weights and procgen target (ADR-8381): paint, attach and detach.
            "terrain.edit_layer_detach" | "terrain.edit_layer_attach" | "terrain.edit_layer_paint_weight" => {
                let method = match action.id {
                    "terrain.edit_layer_paint_weight" => "terrain.edit_layer_paint_weights",
                    _ => "terrain.edit_layer_procgen",
                };
                assert_eq!(action.path, ActionPath::Method(method));
                let terrain = agent.world().spawn(small_terrain()).id();
                let bits = terrain.to_bits();
                let created = agent.call(
                    "terrain.edit_layer_create",
                    json!({ "entity": bits, "name": "Hills", "blend": "add" }),
                );
                assert!(created["layers"][0]["procgen"].is_null(), "{created}");
                match action.id {
                    "terrain.edit_layer_paint_weight" => {
                        let painted = agent.call(
                            "terrain.edit_layer_paint_weights",
                            json!({ "entity": bits, "layer": "Hills", "col": 0, "row": 0, "channels": [0.0, 1.0] }),
                        );
                        let weights = painted["weights"].as_array().expect("weights");
                        let sum: f64 = weights.iter().filter_map(serde_json::Value::as_f64).sum();
                        assert!((sum - 1.0).abs() < 1e-5, "weights sum to 1: {painted}");
                        assert!(
                            (weights[1].as_f64().unwrap_or(0.0) - 0.5).abs() < 1e-5,
                            "{painted}"
                        );
                        let err = agent
                            .try_call(
                                "terrain.edit_layer_paint_weights",
                                json!({
                                    "entity": bits,
                                    "layer": "Hills",
                                    "col": 0,
                                    "row": 0,
                                    "channels": [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
                                }),
                            )
                            .expect_err("more than eight channels");
                        assert_eq!(code(&err), "edit_layer_bad_weight", "{err:?}");
                    }
                    "terrain.edit_layer_attach" | "terrain.edit_layer_detach" => {
                        let attached = agent.call(
                            "terrain.edit_layer_procgen",
                            json!({
                                "entity": bits,
                                "layer": "Hills",
                                "graph": "assets/hills.pgraph.ron",
                                "output": "height",
                            }),
                        );
                        assert_eq!(
                            attached["layers"][0]["procgen"]["graph"], "assets/hills.pgraph.ron",
                            "{attached}"
                        );
                        assert_eq!(attached["layers"][0]["procgen"]["output"], "height", "{attached}");
                        let err = agent
                            .try_call(
                                "terrain.stroke",
                                json!({
                                    "entity": bits,
                                    "edit_layer": "Hills",
                                    "mode": "raise",
                                    "center": [0.0, 0.0],
                                    "radius": 4.0,
                                    "strength": 1.0,
                                }),
                            )
                            .expect_err("an owned layer refuses a brush");
                        assert_eq!(code(&err), "edit_layer_owned", "{err:?}");
                        if action.id == "terrain.edit_layer_detach" {
                            let detached = agent.call(
                                "terrain.edit_layer_procgen",
                                json!({ "entity": bits, "layer": "Hills", "detach": true }),
                            );
                            assert!(detached["layers"][0]["procgen"].is_null(), "{detached}");
                        }
                    }
                    _ => {}
                }
                let _ = agent.world().despawn(terrain);
            }
            // Kit painting (3236): Refresh lists the folder's pieces, Choose piece selects one,
            // Place and Erase paint and clear grid cells (`editor.kit_paint`).
            "kit.paint_refresh" | "kit.paint_piece" | "kit.paint_place" | "kit.paint_erase" => {
                assert_eq!(action.path, ActionPath::Method("editor.kit_paint"));
                allow_heavy(&mut agent);
                std::fs::create_dir_all(dir.join("assets/kit")).expect("kit folder");
                std::fs::write(
                    dir.join("assets/kit/block.gltf"),
                    r#"{"asset":{"version":"2.0"}}"#,
                )
                .expect("piece");
                let out = agent.call("editor.kit_paint", json!({ "folder": "assets/kit" }));
                assert!(
                    out["pieces"]
                        .as_array()
                        .is_some_and(|p| p.contains(&json!("assets/kit/block.gltf"))),
                    "{out}"
                );
                match action.id {
                    "kit.paint_piece" => {
                        let out = agent.call("editor.kit_paint", json!({ "piece": "assets/kit/block.gltf" }));
                        assert_eq!(out["piece"], "assets/kit/block.gltf", "{out}");
                        let err = agent
                            .try_call("editor.kit_paint", json!({ "piece": "assets/kit/missing.gltf" }))
                            .expect_err("not in the folder");
                        assert_eq!(code(&err), "kit_paint_invalid", "{err:?}");
                    }
                    "kit.paint_place" | "kit.paint_erase" => {
                        let row = if action.id == "kit.paint_place" { 10 } else { 20 };
                        let out = agent.call(
                            "editor.kit_paint",
                            json!({ "piece": "assets/kit/block.gltf", "cells": [[0, row], [1, row]], "grid": 3.0 }),
                        );
                        assert_eq!(out["changed"], 2, "{out}");
                        if action.id == "kit.paint_erase" {
                            let out =
                                agent.call("editor.kit_paint", json!({ "op": "erase", "cells": [[1, row]] }));
                            assert_eq!(out["changed"], 1, "{out}");
                        }
                    }
                    _ => {}
                }
            }
            // Create ▸ Blockout brush and Bake to mesh (3240): `editor.blockout_add` adds a brush;
            // `editor.blockout_bake` refuses an empty group and bakes a filled one in a job.
            "create.blockout" => {
                assert_eq!(action.path, ActionPath::Method("editor.blockout_add"));
                let added = agent.call(
                    "editor.blockout_add",
                    json!({ "shape": "box", "op": "add", "group": "parity_add", "translation": [0, 0, 0] }),
                );
                let entity = added["entity"].as_u64().expect("brush entity");
                let world = agent.world();
                let brushes = world
                    .query::<(Entity, &regia_editor::blockout::BlockoutBrush)>()
                    .iter(world)
                    .filter(|(e, _)| e.to_bits() == entity)
                    .count();
                assert_eq!(brushes, 1, "{added}");
            }
            "blockout.bake" => {
                assert_eq!(action.path, ActionPath::Method("editor.blockout_bake"));
                let err = agent
                    .try_call("editor.blockout_bake", json!({ "group": "parity_empty" }))
                    .expect_err("no brushes in the group");
                assert_eq!(code(&err), "blockout_missing", "{err:?}");
                agent.call(
                    "editor.blockout_add",
                    json!({ "shape": "box", "op": "add", "group": "parity_bake", "translation": [0, 0, 0] }),
                );
                let baked = agent.call("editor.blockout_bake", json!({ "group": "parity_bake" }));
                assert!(baked["job"].as_str().is_some(), "{baked}");
                regia_editor::blockout::wait_for_worker(agent.world());
                regia_editor::blockout::tick(agent.world());
                let status = agent.call("agent.job_status", json!({ "id": baked["job"] }));
                assert_eq!(status["state"], "done", "{status}");
            }
            // Outliner row menu ▸ Select similar (3210): `editor.select_similar { by }`; with
            // nothing selected it is refused.
            "outliner.select_similar_kind"
            | "outliner.select_similar_components"
            | "outliner.select_similar_asset"
            | "outliner.select_similar_layer" => {
                assert_eq!(action.path, ActionPath::Method("editor.select_similar"));
                let by = action
                    .id
                    .strip_prefix("outliner.select_similar_")
                    .expect("a select-similar id");
                agent.call("editor.select", json!({ "entities": [] }));
                let err = agent
                    .try_call("editor.select_similar", json!({ "by": by }))
                    .expect_err("nothing selected");
                assert_eq!(reason(&err), "nothing_selected", "{err:?}");
                // Two point lights share a kind (a plain entity has none and matches only itself).
                let a = agent
                    .world()
                    .spawn((
                        regia_scene::prelude::Name::new("Similar row A"),
                        regia_render::PointLight::default(),
                    ))
                    .id()
                    .to_bits();
                agent.world().spawn((
                    regia_scene::prelude::Name::new("Similar row B"),
                    regia_render::PointLight::default(),
                ));
                agent.call("editor.select", json!({ "entities": [a] }));
                let out = agent.call("editor.select_similar", json!({ "by": by }));
                assert_eq!(out["by"], by, "{out}");
                assert!(out["entities"].is_array(), "{out}");
                // The compared entity stays selected; the two lights share a kind.
                assert!(!agent.world().resource::<Selection>().is_empty(), "{out}");
                if by == "kind" {
                    assert!(out["entities"].as_array().is_some_and(|e| e.len() >= 2), "{out}");
                }
            }
            // Outliner row menu ▸ Selection root (3210): `editor.selection_root { entity, set }`
            // marks and unmarks the row as one undo step each.
            "outliner.selection_root" => {
                assert_eq!(action.path, ActionPath::Method("editor.selection_root"));
                let entity = agent.call("editor.spawn", json!({ "name": "Root row" }))["entity"]
                    .as_u64()
                    .expect("entity");
                let before = agent.undo_depth();
                let out = agent.call("editor.selection_root", json!({ "entity": entity, "set": true }));
                assert_eq!(out["root"], true, "{out}");
                assert_eq!(agent.undo_depth(), before + 1);
                let out = agent.call("editor.selection_root", json!({ "entity": entity, "set": false }));
                assert_eq!(out["root"], false, "{out}");
                let err = agent
                    .try_call(
                        "editor.selection_root",
                        json!({ "entity": (1u64 << 40) + 4_000_000 }),
                    )
                    .expect_err("no such entity");
                assert_eq!(reason(&err), "no_entity", "{err:?}");
            }
            // Preview scene ▸ Floor / Rotate (3212): `editor.preview_scene { editor, floor | rotate }`
            // patches one editor's preview settings and leaves the others.
            "preview_scene.floor" | "preview_scene.rotate" => {
                assert_eq!(action.path, ActionPath::Method("editor.preview_scene"));
                let key = action.id.trim_start_matches("preview_scene.");
                let out = agent.call("editor.preview_scene", json!({ "editor": "mesh", key: true }));
                assert_eq!(out[key], true, "{out}");
                assert_eq!(out["editor"], "mesh", "{out}");
                let other = agent.call("editor.preview_scene", json!({ "editor": "material" }));
                assert_eq!(other[key], false, "the material editor keeps its own: {other}");
                let out = agent.call("editor.preview_scene", json!({ "editor": "mesh", key: false }));
                assert_eq!(out[key], false, "{out}");
            }
            // The mesh viewer's and the Content Browser's mesh helpers (3212): `editor.mesh_helper
            // { path, action }`. Lightmap UV unwraps a built-in mesh; collision on a built-in shape
            // and an unknown shape are refused; the nav preview of this level has no geometry.
            "asset_viewer.add_collision"
            | "asset_viewer.lightmap_uv"
            | "asset_viewer.nav_preview"
            | "content.add_collision"
            | "content.lightmap_uv"
            | "content.nav_preview" => {
                assert_eq!(action.path, ActionPath::Method("editor.mesh_helper"));
                let helper = action.id.rsplit('.').next().expect("an action suffix");
                match helper {
                    "lightmap_uv" => {
                        let out = agent.call(
                            "editor.mesh_helper",
                            json!({ "path": "builtin:cube", "action": "lightmap_uv", "size": 64 }),
                        );
                        assert_eq!(out["action"], "lightmap_uv", "{out}");
                        assert_eq!(out["mime"], "image/png", "{out}");
                        assert!(
                            out["png_base64"].as_str().is_some_and(|p| !p.is_empty()),
                            "{}",
                            action.id
                        );
                    }
                    "add_collision" => {
                        let err = agent
                            .try_call(
                                "editor.mesh_helper",
                                json!({ "path": "builtin:cube", "action": "add_collision" }),
                            )
                            .expect_err("a built-in shape has no import settings");
                        assert_eq!(reason(&err), "mesh_helper_not_importable", "{err:?}");
                        let err = agent
                            .try_call(
                                "editor.mesh_helper",
                                json!({ "path": "builtin:cube", "action": "add_collision", "shape": "blob" }),
                            )
                            .expect_err("an unknown shape");
                        assert_eq!(reason(&err), "mesh_helper_unknown_shape", "{err:?}");
                    }
                    _ => {
                        let err = agent
                            .try_call(
                                "editor.mesh_helper",
                                json!({ "path": "builtin:cube", "action": "nav_preview" }),
                            )
                            .expect_err("this level has no static collision");
                        assert_eq!(code(&err), "nav_preview_no_geometry", "{err:?}");
                    }
                }
                let err = agent
                    .try_call(
                        "editor.mesh_helper",
                        json!({ "path": "builtin:cube", "action": "polish" }),
                    )
                    .expect_err("an unknown helper");
                assert_eq!(reason(&err), "mesh_helper_unknown_action", "{err:?}");
            }
            // Safe mode banner (3266): Restart normally is `editor.safe_mode_restart` (refused
            // outside safe mode); Continue in safe mode only closes the prompt.
            "safe_mode.continue" => assert_eq!(action.path, ActionPath::ViewOnly),
            "safe_mode.restart" => {
                assert_eq!(action.path, ActionPath::Method("editor.safe_mode_restart"));
                agent
                    .world()
                    .resource_mut::<regia_agent::AgentPermissions>()
                    .config
                    .permissions
                    .insert("tools".into());
                let err = agent
                    .try_call("editor.safe_mode_restart", json!({}))
                    .expect_err("not in safe mode");
                assert_eq!(reason(&err), "safe_mode_inactive", "{err:?}");
                agent
                    .world()
                    .insert_resource(regia_editor::safe_mode::SafeMode::new(true, false));
                let out = agent.call("editor.safe_mode_restart", json!({}));
                assert_eq!(out["restart_requested"], true, "{out}");
                assert_eq!(out["enabled"], true, "{out}");
                agent
                    .world()
                    .remove_resource::<regia_editor::safe_mode::SafeMode>();
            }
            // Layers panel (3267): Lock is `editor.layer_lock { layer, on }`; Store in its own file
            // and Merge back are `editor.layer_file { layer, separate }` on the saved host. This
            // level declares no layers, so each names the refusal for an unknown layer.
            "layers.lock" => {
                assert_eq!(action.path, ActionPath::Method("editor.layer_lock"));
                let err = agent
                    .try_call("editor.layer_lock", json!({ "layer": "nope", "on": true }))
                    .expect_err("no such layer");
                assert_eq!(code(&err), "layer_unknown", "{err:?}");
                let out = agent.call("editor.levels_open", json!({}));
                assert_eq!(
                    out["locked_layers"],
                    json!([]),
                    "a refused lock locks nothing: {out}"
                );
            }
            "layers.own_file" | "layers.merge_file" => {
                assert_eq!(action.path, ActionPath::Method("editor.layer_file"));
                agent.call(
                    "editor.save_level",
                    json!({ "path": "assets/maps/layers_host.level.ron" }),
                );
                let separate = action.id == "layers.own_file";
                let err = agent
                    .try_call(
                        "editor.layer_file",
                        json!({ "layer": "night", "separate": separate }),
                    )
                    .expect_err("this level has no `night` layer");
                let want = if separate {
                    "layer_unknown"
                } else {
                    "layer_not_separated"
                };
                assert_eq!(code(&err), want, "{err:?}");
            }
            // Levels (3267): Open beside, Save, Close and Active (`editor.levels_open { op }`,
            // `editor.active_level`) on a second level beside the host.
            "levels.open" | "levels.save" | "levels.close" | "levels.active" => {
                let method = if action.id == "levels.active" {
                    "editor.active_level"
                } else {
                    "editor.levels_open"
                };
                assert_eq!(action.path, ActionPath::Method(method));
                let file = format!("assets/maps/beside_{}.level.ron", action.id.replace('.', "_"));
                let beside = dir.join(&file);
                std::fs::write(
                    &beside,
                    format!(
                        "SceneFile(format_version: {}, entities: [], resources: {{}})",
                        regia_scene::format::SCENE_FORMAT_VERSION
                    ),
                )
                .expect("beside level");
                let path = beside.display().to_string();
                let state = agent.call("editor.levels_open", json!({ "op": "open", "path": path }));
                let document = |state: &Value| -> Option<Value> {
                    state["documents"].as_array().and_then(|docs| {
                        docs.iter()
                            .find(|d| d["file"].as_str().is_some_and(|f| f.ends_with(file.as_str())))
                            .cloned()
                    })
                };
                let doc = document(&state).unwrap_or_else(|| panic!("opened beside: {state}"));
                assert_eq!(doc["kind"], "level", "{state}");
                match action.id {
                    "levels.open" => {
                        let err = agent
                            .try_call("editor.levels_open", json!({ "op": "open", "path": path }))
                            .expect_err("open twice");
                        assert_eq!(code(&err), "level_already_open", "{err:?}");
                    }
                    "levels.save" => {
                        let out = agent.call(
                            "editor.levels_open",
                            json!({ "op": "save", "document": doc["id"] }),
                        );
                        assert!(out["written"].is_array(), "{out}");
                        let saved = document(&out).expect("still open");
                        assert_eq!(saved["dirty"], false, "{out}");
                    }
                    "levels.active" => {
                        let out = agent.call("editor.active_level", json!({ "set": path }));
                        assert_eq!(out["active"], doc["level"], "{out}");
                        let out = agent.call("editor.active_level", json!({ "set": "host" }));
                        assert_eq!(out["active"], out["host"], "{out}");
                        let err = agent
                            .try_call("editor.active_level", json!({ "set": "no/such.level.ron" }))
                            .expect_err("not open");
                        assert_eq!(code(&err), "level_not_open", "{err:?}");
                    }
                    _ => {
                        let err = agent
                            .try_call("editor.levels_open", json!({ "op": "close", "level": "host" }))
                            .expect_err("the host closes with File ▸ Open or New");
                        assert_eq!(code(&err), "host_level", "{err:?}");
                    }
                }
                let out = agent.call(
                    "editor.levels_open",
                    json!({ "op": "close", "level": path, "discard": true }),
                );
                assert!(document(&out).is_none(), "closed: {out}");
            }
            // World Map (3238): Open needs a streamed level's directory form; Load / Unload need an
            // open world; Cancel load names a job the editor never started. Clear selection and
            // Keep loaded are view state.
            "world_map.select_none" | "world_map.unload_keep" => {
                assert_eq!(action.path, ActionPath::ViewOnly);
            }
            "world_map.open" => {
                assert_eq!(action.path, ActionPath::Method("editor.world_open"));
                let file = dir.join("assets/maps/world_flat.level.ron");
                std::fs::write(
                    &file,
                    format!(
                        "SceneFile(format_version: {}, entities: [], resources: {{}})",
                        regia_scene::format::SCENE_FORMAT_VERSION
                    ),
                )
                .expect("single-file level");
                let err = agent
                    .try_call(
                        "editor.world_open",
                        json!({ "path": "assets/maps/world_flat.level.ron" }),
                    )
                    .expect_err("a single-file level is not streamed");
                assert_eq!(reason(&err), "world_needs_level_dir", "{err:?}");
                let out = agent.call("editor.world_cells", json!({}));
                assert!(out.is_object(), "{out}");
            }
            "world_map.load_selected" | "world_map.load_around_camera" => {
                assert_eq!(action.path, ActionPath::Method("editor.world_load"));
                let params = if action.id == "world_map.load_selected" {
                    json!({ "cells": ["0_0"] })
                } else {
                    json!({ "radius_m": 50.0 })
                };
                let err = agent
                    .try_call("editor.world_load", params)
                    .expect_err("no world is open");
                assert_eq!(reason(&err), "world_not_open", "{err:?}");
                if action.id == "world_map.load_selected" {
                    let err = agent
                        .try_call("editor.world_load", json!({ "cells": [] }))
                        .expect_err("no cells");
                    assert_eq!(reason(&err), "world_cells_empty", "{err:?}");
                }
            }
            "world_map.unload_selected" | "world_map.unload_save" | "world_map.unload_discard" => {
                let want = if action.id == "world_map.unload_selected" {
                    ActionPath::Method("editor.world_unload")
                } else {
                    ActionPath::Confirm("editor.world_unload")
                };
                assert_eq!(action.path, want);
                let save = match action.id {
                    "world_map.unload_save" => json!(true),
                    "world_map.unload_discard" => json!(false),
                    _ => Value::Null,
                };
                let err = agent
                    .try_call("editor.world_unload", json!({ "cells": ["0_0"], "save": save }))
                    .expect_err("no world is open");
                assert_eq!(reason(&err), "world_not_open", "{err:?}");
            }
            "world_map.load_cancel" => {
                assert_eq!(action.path, ActionPath::Method("editor.cancel_job"));
                let err = agent
                    .try_call("editor.cancel_job", json!({ "id": "world_load:999" }))
                    .expect_err("no load runs");
                assert_eq!(reason(&err), "job_not_found", "{err:?}");
            }
            // Collaboration (3260): Host listens on loopback, Join sends a request, Leave ends
            // either; Accept / Reject need a pending joiner and Jump an accepted participant.
            "collab.host" | "collab.join" | "collab.leave" | "collab.accept" | "collab.reject"
            | "collab.jump" => {
                let method = match action.id {
                    "collab.join" => "editor.collab_join",
                    "collab.leave" => "editor.collab_leave",
                    "collab.jump" => "editor.collab_jump",
                    _ => "editor.collab_host",
                };
                assert_eq!(action.path, ActionPath::Method(method));
                let off = agent.call("editor.collab_participants", json!({}));
                assert_eq!(off["state"], "off", "{off}");
                match action.id {
                    "collab.join" => {
                        let err = agent
                            .try_call(
                                "editor.collab_join",
                                json!({ "address": "127.0.0.1:0", "name": "Guest" }),
                            )
                            .expect_err("join needs a port");
                        assert_eq!(code(&err), "collab_bad_address", "{err:?}");
                        let out = agent.call(
                            "editor.collab_join",
                            json!({ "address": "127.0.0.1:47999", "name": "Guest" }),
                        );
                        assert_eq!(out["state"], "joining", "{out}");
                    }
                    "collab.jump" => {
                        let err = agent
                            .try_call("editor.collab_jump", json!({ "participant": 7 }))
                            .expect_err("no such participant");
                        assert_eq!(code(&err), "collab_unknown_participant", "{err:?}");
                    }
                    _ => {
                        let out = agent.call(
                            "editor.collab_host",
                            json!({ "address": "127.0.0.1:0", "name": "Parity host" }),
                        );
                        assert_eq!(out["state"], "hosting", "{out}");
                        assert!(out["address"].as_str().is_some(), "{out}");
                        if action.id == "collab.accept" || action.id == "collab.reject" {
                            let verb = action.id.trim_start_matches("collab.");
                            let err = agent
                                .try_call("editor.collab_host", json!({ "action": verb, "participant": 2 }))
                                .expect_err("nobody is waiting");
                            assert_eq!(code(&err), "collab_not_pending", "{err:?}");
                        }
                    }
                }
                let out = agent.call("editor.collab_leave", json!({}));
                assert_eq!(out["state"], "off", "{out}");
            }
            // Profiler ▸ Memory (ADR-8142): the view switch is view-only; Take, Diff from / to and
            // Export run `editor.memory_snapshot`; the GPU browser's controls run
            // `editor.gpu_resources` (list works headless; preview needs a render device).
            "profiler.memory_view" => assert_eq!(action.path, ActionPath::ViewOnly),
            "profiler.memory_take"
            | "profiler.memory_set_a"
            | "profiler.memory_set_b"
            | "profiler.memory_export" => {
                assert_eq!(action.path, ActionPath::Method("editor.memory_snapshot"));
                let a = format!("{} a", action.id);
                let b = format!("{} b", action.id);
                let out = agent.call("editor.memory_snapshot", json!({ "action": "take", "name": a }));
                assert_eq!(out["snapshot"]["name"], json!(a), "{out}");
                agent.world().spawn_empty();
                agent.call("editor.memory_snapshot", json!({ "action": "take", "name": b }));
                let out = agent.call(
                    "editor.memory_snapshot",
                    json!({ "action": "diff", "a": a, "b": b }),
                );
                assert_eq!(out["diff"]["a"], json!(a), "{out}");
                assert!(out["diff"]["entities"].as_i64().is_some_and(|n| n >= 1), "{out}");
                let out = agent.call("editor.memory_snapshot", json!({ "action": "list" }));
                assert_eq!(
                    out["diff"],
                    json!({ "a": a, "b": b }),
                    "the diff sides follow the call: {out}"
                );
                if action.id == "profiler.memory_export" {
                    let out = agent.call(
                        "editor.memory_snapshot",
                        json!({ "action": "export", "name": a, "path": "captures/memory/parity.json" }),
                    );
                    assert!(dir.join("captures/memory/parity.json").is_file(), "{out}");
                }
                let err = agent
                    .try_call("editor.memory_snapshot", json!({ "action": "take", "name": a }))
                    .expect_err("the name is taken");
                assert_eq!(code(&err), "memory_name_taken", "{err:?}");
            }
            "profiler.gpu_mip_down"
            | "profiler.gpu_mip_up"
            | "profiler.gpu_channel"
            | "profiler.gpu_live"
            | "profiler.gpu_refresh" => {
                assert_eq!(action.path, ActionPath::Method("editor.gpu_resources"));
                let out = agent.call("editor.gpu_resources", json!({ "action": "list" }));
                assert!(out["resources"].is_array(), "{out}");
                let err = agent
                    .try_call(
                        "editor.gpu_resources",
                        json!({ "action": "preview", "label": "hdr color", "mip": 1, "channel": "r" }),
                    )
                    .expect_err("headless: no render device");
                assert_eq!(code(&err), "gpu_no_device", "{err:?}");
            }
            // Import presets and the import dialog (w12-import-dialog, ADR-7904): each button is its
            // method, checked on fresh files per action (a second import of one name is refused).
            "import_settings.preset_save"
            | "import_settings.preset_apply"
            | "import_settings.preset_delete"
            | "import_dialog.ok"
            | "import_dialog.apply_to_all"
            | "import_dialog.skip"
            | "import_dialog.cancel"
            | "import_dialog.preset_apply"
            | "import_dialog.clip_import"
            | "import_dialog.clip_add_range"
            | "import_dialog.clip_remove_range"
            | "import_dialog.mesh_import"
            | "content.reimport_with_options" => import_dialog_check(&mut agent, &dir, action),
            // Profiler ▸ Lockstep part toggles (ADR-8562): `sim.det_debug_draw { what }`.
            "profiler.lockstep_grid"
            | "profiler.lockstep_occupancy"
            | "profiler.lockstep_candidates"
            | "profiler.lockstep_contacts"
            | "profiler.lockstep_queries" => {
                assert_eq!(action.path, ActionPath::Method("sim.det_debug_draw"));
                let part = action.id.trim_start_matches("profiler.lockstep_");
                if agent
                    .world()
                    .get_resource::<regia_framework::det_debug::DetDebugDraw>()
                    .is_none()
                {
                    let err = agent
                        .try_call("sim.det_debug_draw", json!({ "what": [part] }))
                        .expect_err("no det debug source yet");
                    assert_eq!(code(&err), "no_det_debug", "{err:?}");
                    agent
                        .world()
                        .insert_resource(regia_framework::det_debug::DetDebugDraw::default());
                }
                let out = agent.call("sim.det_debug_draw", json!({ "what": [part] }));
                assert_eq!(out["what"], json!([part]), "{out}");
                let out = agent.call("sim.det_debug_draw", json!({ "what": "none" }));
                assert_eq!(out["what"], json!([]), "{out}");
            }
            "world_settings.match_rules" => {
                // World Settings ▸ Edit rules… (ADR-8738): editor.rules_open without a path only
                // brings the Match Rules panel forward.
                assert_eq!(action.path, ActionPath::Method("editor.rules_open"));
                let out = agent.call("editor.rules_open", json!({}));
                assert_eq!(out["rules"], Value::Null, "{out}");
                assert!(out["game_rules"]["rule_sets"].is_array(), "{out}");
            }
            other => panic!("panel action {other} has no headless check here"),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
/// A panel open/close must not add empty rows to the scene snapshot. Real empty scene entities
/// are deliberately retained, so the smoke harness still detects an authored-entity leak.
#[test]
fn smoke_panel_open_close_keeps_empty_scene_entity_count() {
    let root = project();
    let mut agent = Agent::new(&root);
    let empty = agent.world().spawn_empty().id();
    let before = agent.call("scene.serialize", json!({}));
    let actual_empty_before = agent
        .world()
        .iter_entities()
        .filter(|e| e.archetype().components().is_empty())
        .count();
    let known: std::collections::BTreeSet<_> = agent.world().iter_entities().map(|e| e.id()).collect();
    for _ in 0..3 {
        agent.command("view.toggle_frame_debugger");
        agent.command("view.toggle_frame_debugger");
        assert!(
            !regia_editor::frame_debugger::state(agent.world())["open"]
                .as_bool()
                .expect("open")
        );
        let after = agent.call("scene.serialize", json!({}));
        let world = agent.world();
        assert_eq!(
            world
                .iter_entities()
                .filter(|e| e.archetype().components().is_empty())
                .count(),
            actual_empty_before
        );
        for entity in world.iter_entities().filter(|e| !known.contains(&e.id())) {
            let components: Vec<_> = entity
                .archetype()
                .components()
                .iter()
                .filter_map(|id| world.components().get_name(*id))
                .collect();
            eprintln!("panel allocated {:?}: {components:?}", entity.id());
            assert!(entity.contains::<regia_ecs::ecs::resource::IsResource>());
        }
        assert_eq!(
            after["entities"], before["entities"],
            "panel open/close added scene rows"
        );
        assert_eq!(after["ron"], before["ron"], "panel open/close changed the scene");
    }
    assert!(
        before["ron"]
            .as_str()
            .expect("ron")
            .contains(&format!("// entity {} ", empty.to_bits()))
    );
    agent.world().despawn(empty);
    let after = agent.call("scene.serialize", json!({}));
    assert_eq!(
        after["entities"].as_u64(),
        before["entities"].as_u64().map(|n| n - 1)
    );
    drop(agent);
    std::fs::remove_dir_all(root).expect("cleanup");
}

// Also run the new actions independently: an unrelated earlier registry action must not hide
// their dispatcher coverage when the full sweep reports a failure.
fn library_shell_action(action: &regia_editor::agent_parity::EditorAction) {
    assert_eq!(action.path, ActionPath::Method("editor.asset_library"));
    let library_project = regia_test_utils::unique_temp_dir("library_shell_project");
    std::fs::create_dir_all(&library_project).expect("project");
    let mut agent = Agent::new(&library_project);
    let config = regia_test_utils::unique_temp_dir("library_shell");
    agent
        .world()
        .insert_resource(regia_editor::editor_prefs::EditorConfigPaths::at(&config));
    std::fs::create_dir_all(library_project.join("assets")).expect("assets");
    regia_test_utils::write_atomic(&library_project.join("assets/library-test.png"), b"image")
        .expect("asset");
    let listed = agent.library_call(json!({"action": "list"}));
    assert_eq!(listed["entries"], json!([]));
    if action.id != "library.list" {
        let added = agent.library_call(json!({"action": "add", "path": "assets/library-test.png"}));
        assert_eq!(added["assets"], 1);
        if action.id == "library.use" {
            let used = agent.library_call(json!({"action": "use", "entry": added["entry"]}));
            let copied = used["roots"][0].as_str().expect("copied path");
            assert_eq!(
                std::fs::read(library_project.join(copied)).expect("copied"),
                b"image"
            );
        }
    }
    let err = agent
        .try_call(
            "editor.asset_library",
            json!({"action": "use", "entry": "../outside"}),
        )
        .expect_err("refuse escape");
    assert_eq!(code(&err), "library_path");
    let _ = std::fs::remove_dir_all(&config);
    drop(agent);
    let _ = std::fs::remove_dir_all(&library_project);
}

#[test]
fn asset_library_actions_run_through_the_dispatcher() {
    let actions: Vec<_> = ACTIONS
        .iter()
        .filter(|action| matches!(action.id, "library.list" | "library.add" | "library.use"))
        .collect();
    assert_eq!(actions.len(), 3);
    for action in actions {
        library_shell_action(action);
    }
}

#[test]
fn help_completion_and_settings_reset_cross_the_dispatcher() {
    let root = regia_test_utils::unique_temp_dir("help-settings-dispatch");
    std::fs::create_dir_all(&root).expect("root");
    let original = "[project]\nname='Demo'\nid='com.example.demo'\nversion='3.0.0'\nbuild_number=7\n";
    let staged = root.join("Regia.toml.tmp");
    std::fs::write(&staged, original).expect("staged manifest");
    std::fs::rename(&staged, root.join("Regia.toml")).expect("manifest");
    let mut agent = Agent::new(&root);
    let completion = agent.call("editor.content_index", json!({"complete": "t"}));
    assert_eq!(completion["candidates"], json!(["t:", "type:"]));
    let help = agent.call(
        "editor.help_context",
        json!({"field": {"component": "UtilityReasoner", "path": "score"}}),
    );
    assert_eq!(help["anchor"], "on-entities-utilityreasoner");
    let prefs = agent.call("editor.prefs_get", json!({"query": "appearance.ui_scale"}));
    assert_eq!(prefs["prefs"][0]["changed"], false);
    assert_eq!(prefs["prefs"][0]["default"], 1.0);
    let before = agent.call("editor.project_settings_get", json!({}));
    assert!(
        before["settings"]
            .as_array()
            .expect("rows")
            .iter()
            .any(|row| row["key"] == "project.version" && row["changed"] == true)
    );
    agent.call(
        "editor.project_settings_set",
        json!({"reset_category": "project"}),
    );
    assert_eq!(agent.world().resource::<UndoStack>().undo_depth(), 1);
    let after = agent.call("editor.project_settings_get", json!({}));
    assert_eq!(after["project"]["version"], "0.1.0");
    assert_eq!(after["project"]["build_number"], 1);
    agent.call("editor.undo", json!({}));
    assert_eq!(
        std::fs::read_to_string(root.join("Regia.toml")).expect("restored manifest"),
        original
    );
    drop(agent);
    let _ = std::fs::remove_dir_all(root);
}

/// The overlay method crosses the actual request dispatcher, including the permission and
/// schema gates. Presets restore a layout without touching scene undo history.
#[test]
fn viewport_overlays_dispatch_presets_and_reject_invalid_edits_atomically() {
    let root = regia_test_utils::unique_temp_dir("overlay-dispatch");
    std::fs::create_dir_all(&root).expect("project");
    let mut agent = Agent::new(&root);
    agent
        .world()
        .insert_resource(regia_editor::editor_prefs::EditorConfigPaths::at(
            root.join("user"),
        ));
    let before = agent.world().resource::<UndoStack>().undo_depth();
    let changed = agent.call(
        "editor.viewport_overlays",
        json!({"action":"set", "pane":"top", "group":"snap", "dock":"left", "vertical":true}),
    );
    agent.call(
        "editor.viewport_overlays",
        json!({"action":"preset_save", "pane":"top", "name":"Left snap"}),
    );
    agent.call(
        "editor.viewport_overlays",
        json!({"action":"reset", "pane":"top"}),
    );
    let applied = agent.call(
        "editor.viewport_overlays",
        json!({"action":"preset_apply", "pane":"top", "name":"Left snap"}),
    );
    assert_eq!(applied["groups"], changed["groups"]);
    let error = agent.try_call("editor.viewport_overlays", json!({"action":"set", "pane":"top", "group":"snap", "dock":"right", "position":[-1,0], "selection":"lasso"})).expect_err("bad position");
    assert_eq!(error.data.expect("data")["code"], "viewport_overlay_invalid");
    assert_eq!(
        agent.call("editor.viewport_overlays", json!({"pane":"top"})),
        applied
    );
    assert!(!regia_editor::selection::SelectionShape::lasso_on(agent.world()));
    assert_eq!(agent.world().resource::<UndoStack>().undo_depth(), before);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn workspace_area_mutations_require_edit_permission() {
    let dir = project();
    let mut agent = Agent::new(&dir);
    agent
        .world()
        .resource_mut::<regia_agent::AgentPermissions>()
        .config
        .permissions
        .remove("edit_scene");
    for (method, params) in [
        (
            "editor.layout_join",
            json!({"from": "viewport", "into": "inspector"}),
        ),
        (
            "editor.layout_swap",
            json!({"from": "viewport", "with": "inspector"}),
        ),
        (
            "editor.workspace_set",
            json!({"id": "terrain", "mode": "terrain"}),
        ),
    ] {
        let error = agent
            .try_call(method, params)
            .expect_err("edit permission required");
        assert_eq!(error.code, regia_agent::protocol::PERMISSION_DENIED);
        assert!(
            agent
                .world()
                .get_resource::<regia_editor::workspaces::ViewRequests>()
                .is_none_or(|r| r.pending.is_empty())
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Isolated coverage so an unrelated earlier shell action cannot hide the new routes.
#[test]
fn workspace_modes_and_area_actions_run_through_the_dispatcher() {
    use regia_editor::workspaces::{DockGroups, DockViews};
    let dir = project();
    let mut agent = Agent::new(&dir);
    let mut views = DockViews::default();
    let mut dock = regia_editor::editor_layout::default_dock();
    for (workspace, active) in [("terrain", true), ("level", false)] {
        agent.call("editor.workspace_set", json!({"id": workspace}));
        views.apply_requests(&mut dock, agent.world());
        views.publish(agent.world());
        assert_eq!(
            agent.call("editor.workspace_list", json!({}))["mode"],
            if active { "terrain" } else { "world" }
        );
        assert_eq!(
            agent
                .world()
                .resource::<regia_editor::terrain_tool::TerrainTool>()
                .active,
            active
        );
    }
    for method in ["editor.layout_join", "editor.layout_swap"] {
        let mut dock = egui_dock::DockState::new(vec![Panel::Viewport, Panel::Content]);
        dock.main_surface_mut().split_right(
            egui_dock::NodeIndex::root(),
            0.6,
            vec![Panel::Inspector, Panel::Outliner],
        );
        DockGroups::refresh(agent.world(), &dock);
        let before = DockGroups::of(&dock);
        let params = if method == "editor.layout_join" {
            json!({"from": "inspector", "into": "viewport"})
        } else {
            json!({"from": "viewport", "with": "inspector"})
        };
        agent.call(method, params);
        views.apply_requests(&mut dock, agent.world());
        let after = DockGroups::of(&dock);
        if method == "editor.layout_join" {
            assert_eq!(after.groups.len(), 1);
            assert_eq!(
                after.groups.values().next().expect("one area"),
                &vec![Panel::Viewport, Panel::Content, Panel::Inspector, Panel::Outliner]
            );
        } else {
            let keys: Vec<_> = before.groups.keys().collect();
            assert_eq!(after.groups[keys[0]], before.groups[keys[1]]);
            assert_eq!(after.groups[keys[1]], before.groups[keys[0]]);
        }
    }
    agent.call("editor.layout_apply", json!({"open": "terrain"}));
    assert!(
        agent
            .layout_requests()
            .contains(&LayoutRequest::Open(Panel::Terrain))
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Verify this lane's shell action even if an unrelated earlier registry row fails.
#[test]
fn play_device_safe_area_action_runs_headless() {
    let dir = project();
    let mut agent = Agent::new(&dir);
    let action = ACTIONS
        .iter()
        .find(|action| action.id == "play.device_safe_area")
        .expect("registered action");
    let ActionPath::Method(method) = action.path else {
        panic!("safe-area action needs an agent method");
    };
    agent
        .world()
        .insert_resource(regia_editor::play_window::PlayWindowSupport);
    let out = agent.call(
        method,
        json!({"window": true, "device": {"preset": "phone", "safe_area_outline": true}}),
    );
    assert_eq!(out["window"], "play_window");
    assert!(
        agent
            .world()
            .resource::<regia_editor::play_device::DeviceSim>()
            .safe_area_outline
    );
    let state = agent.call("editor.play_state", json!({}));
    assert_eq!(state["device"]["safe_area_outline"], true);
    drop(agent);
    let _ = std::fs::remove_dir_all(dir);
}

/// The headless check of one Import settings preset button or import dialog button (ADR-7904):
/// the action's method, on files written for this action only.
fn import_dialog_check(
    agent: &mut Agent,
    dir: &std::path::Path,
    action: &regia_editor::agent_parity::EditorAction,
) {
    let slug = action.id.replace('.', "_");
    let incoming = dir.join(format!("incoming_{slug}"));
    std::fs::create_dir_all(&incoming).expect("incoming");
    let write = |name: &str, bytes: &[u8]| {
        let path = incoming.join(name);
        std::fs::write(&path, bytes).expect("source");
        path.to_string_lossy().into_owned()
    };
    let folder = format!("parity_{slug}");
    let asset = |name: &str| format!("assets/{folder}/{name}");
    let meta = |rel: &str| {
        let bytes = std::fs::read(dir.join(format!("{rel}.meta"))).expect("meta");
        regia_asset::MetaSidecar::from_ron(&bytes).expect("meta parses")
    };
    let subs = |rel: &str| -> Vec<String> { meta(rel).sub_assets.iter().map(|s| s.path.clone()).collect() };
    let preset_names = |out: &Value| -> Vec<String> {
        out["presets"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| r["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let save_preset = |agent: &mut Agent| {
        agent.call(
            "editor.import_presets",
            json!({ "action": "save", "name": "Parity small", "importer": "texture", "settings": "(max_size: Some(64))" }),
        )
    };
    let small = |rel: &str| {
        meta(rel)
            .settings
            .is_some_and(|s| s.contains("max_size: Some(64)"))
    };
    match action.id {
        "import_settings.preset_save" => {
            assert_eq!(action.path, ActionPath::Method("editor.import_presets"));
            let out = save_preset(agent);
            assert!(preset_names(&out).iter().any(|n| n == "Parity small"), "{out}");
        }
        "import_settings.preset_delete" => {
            save_preset(agent);
            let out = agent.call(
                "editor.import_presets",
                json!({ "action": "delete", "name": "Parity small", "importer": "texture" }),
            );
            assert!(!preset_names(&out).iter().any(|n| n == "Parity small"), "{out}");
        }
        "import_settings.preset_apply" => {
            save_preset(agent);
            let png = write("apply.png", &parity_png());
            agent.call("editor.import_with", json!({ "paths": [png], "folder": folder }));
            let state = agent.call(
                "editor.import_presets",
                json!({ "action": "apply", "name": "Parity small", "paths": [asset("apply.png")] }),
            );
            let field = state["fields"]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["key"] == "max_size"))
                .expect("max_size row");
            assert_eq!(field["set"], true, "the preset filled the form: {state}");
        }
        "import_dialog.ok" => {
            assert_eq!(action.path, ActionPath::Method("editor.import_with"));
            let png = write("ok.png", &parity_png());
            let out = agent.call("editor.import_with", json!({ "paths": [png], "folder": folder }));
            assert_eq!(out["imported"], json!([asset("ok.png")]), "{out}");
            assert_eq!(out["errors"], json!([]), "{out}");
        }
        "import_dialog.apply_to_all" => {
            let paths = [write("a.png", &parity_png()), write("b.png", &parity_png())];
            let state = agent.call(
                "editor.import_dialog_open",
                json!({ "paths": paths, "folder": folder }),
            );
            assert_eq!(state["open"], true, "{state}");
            let out = agent.call(
                "editor.import_with",
                json!({ "settings": "(max_size: Some(64))", "apply_to_all": true }),
            );
            assert_eq!(out["imported"].as_array().map(Vec::len), Some(2), "{out}");
            assert!(small(&asset("a.png")) && small(&asset("b.png")), "{out}");
        }
        "import_dialog.skip" => {
            let paths = [write("keep.png", &parity_png()), write("skip.png", &parity_png())];
            let out = agent.call(
                "editor.import_with",
                json!({ "paths": paths, "folder": folder, "skip": ["skip.png"] }),
            );
            assert_eq!(out["imported"], json!([asset("keep.png")]), "{out}");
            assert!(
                !dir.join(asset("skip.png")).exists(),
                "the skipped file stays out"
            );
        }
        "import_dialog.cancel" => {
            assert_eq!(action.path, ActionPath::Method("editor.import_dialog_open"));
            let paths = [write("a.png", &parity_png()), write("b.png", &parity_png())];
            let state = agent.call(
                "editor.import_dialog_open",
                json!({ "paths": paths, "folder": folder }),
            );
            assert_eq!(state["open"], true, "{state}");
            let state = agent.call("editor.import_dialog_open", json!({ "close": true }));
            assert_eq!(state["open"], false, "{state}");
            assert!(!dir.join(asset("a.png")).exists(), "Cancel imports nothing");
        }
        "import_dialog.preset_apply" => {
            save_preset(agent);
            let png = write("preset.png", &parity_png());
            let out = agent.call(
                "editor.import_with",
                json!({ "paths": [png], "folder": folder, "preset": "Parity small" }),
            );
            assert_eq!(out["imported"], json!([asset("preset.png")]), "{out}");
            assert!(small(&asset("preset.png")), "{out}");
        }
        "import_dialog.clip_import" => {
            let glb = write("hero.glb", &parity_two_take_glb());
            let out = agent.call(
                "editor.import_with",
                json!({ "paths": [glb], "folder": folder, "items": [{ "kind": "clip", "animation": "b", "import": false }] }),
            );
            assert_eq!(out["errors"], json!([]), "{out}");
            let subs = subs(&asset("hero.glb"));
            assert!(subs.iter().any(|p| p == "clip/a"), "{subs:?}");
            assert!(!subs.iter().any(|p| p == "clip/b"), "the unticked take: {subs:?}");
        }
        "import_dialog.clip_add_range" | "import_dialog.clip_remove_range" => {
            // Add range splits the take; Remove range sends the ranges that are left.
            let ranges = if action.id == "import_dialog.clip_add_range" {
                json!([
                    { "name": "rise", "start_frame": 0, "end_frame": 15 },
                    { "name": "top", "start_frame": 15, "end_frame": 30 }
                ])
            } else {
                json!([{ "name": "rise", "start_frame": 0, "end_frame": 15 }])
            };
            let glb = write("hero.glb", &parity_two_take_glb());
            let out = agent.call(
                "editor.import_with",
                json!({ "paths": [glb], "folder": folder, "items": [{ "kind": "clip", "animation": "a", "ranges": ranges }] }),
            );
            assert_eq!(out["errors"], json!([]), "{out}");
            let subs = subs(&asset("hero.glb"));
            assert!(subs.iter().any(|p| p == "clip/rise"), "{subs:?}");
            assert_eq!(
                subs.iter().any(|p| p == "clip/top"),
                action.id == "import_dialog.clip_add_range",
                "{subs:?}"
            );
            assert!(!subs.iter().any(|p| p == "clip/a"), "the split take: {subs:?}");
        }
        "content.reimport_with_options" => {
            let glb = write("hero.glb", &parity_two_take_glb());
            agent.call("editor.import_with", json!({ "paths": [glb], "folder": folder }));
            let out = agent.call(
                "editor.import_with",
                json!({ "reimport": asset("hero.glb"), "items": [{ "animation": "b", "name": "jump" }] }),
            );
            assert!(out["cooked"].as_u64().is_some_and(|c| c >= 1), "{out}");
            let subs = subs(&asset("hero.glb"));
            assert!(subs.iter().any(|p| p == "clip/jump"), "{subs:?}");
        }
        // w13-ui-import-mesh-options (ADR-8376): a glTF mesh's Import toggle is `editor.import_with`;
        // headless, Cancel closes the dialog and an import without one is refused.
        "import_dialog.mesh_import" => {
            assert_eq!(action.path, ActionPath::Method("editor.import_with"));
            let out = agent.call("editor.import_dialog_open", json!({ "close": true }));
            assert_eq!(out["open"], false, "{out}");
            let err = agent
                .try_call("editor.import_with", json!({}))
                .expect_err("no import dialog is open");
            assert_eq!(reason(&err), "import_dialog_not_open", "{err:?}");
        }
        other => panic!("import action {other} has no headless check here"),
    }
    let _ = std::fs::remove_dir_all(&incoming);
}

/// A 1 × 1 red PNG.
fn parity_png() -> Vec<u8> {
    use image::ImageEncoder;
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&[255u8, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
        .expect("png encode");
    bytes
}

/// GLB: one triangle mesh with a material, a two-joint skin and two one-second takes `a` and `b`
/// on the bone (30 frames each at the clip rate); the import dialog's own fixture.
fn parity_two_take_glb() -> Vec<u8> {
    let f = |values: &[f32]| values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
    let mut bin = Vec::new();
    bin.extend(f(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]));
    bin.extend(f(&[0.0, 1.0]));
    bin.extend(f(&[0.0, 0.0, 0.0, 0.0, 2.0, 0.0]));
    bin.extend(f(&[0.0, 1.0, 0.0, 0.0, 3.0, 0.0]));
    for i in [0u32, 1, 2] {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0,2]}],
        "nodes":[{"name":"root","children":[1]},{"name":"bone","translation":[0.0,0.0,0.0]},
            {"name":"body","mesh":0}],
        "meshes":[{"name":"body","primitives":[{"attributes":{"POSITION":0},"indices":4,"material":0}]}],
        "materials":[{"name":"paint","pbrMetallicRoughness":{"baseColorFactor":[0.5,0.25,1.0,1.0]}}],
        "skins":[{"joints":[0,1]}],
        "animations":[
            {"name":"a","channels":[{"sampler":0,"target":{"node":1,"path":"translation"}}],
              "samplers":[{"input":1,"output":2,"interpolation":"LINEAR"}]},
            {"name":"b","channels":[{"sampler":0,"target":{"node":1,"path":"translation"}}],
              "samplers":[{"input":1,"output":3,"interpolation":"LINEAR"}]}],
        "accessors":[
            {"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0.0,0.0,0.0],"max":[1.0,1.0,0.0]},
            {"bufferView":1,"componentType":5126,"count":2,"type":"SCALAR","min":[0.0],"max":[1.0]},
            {"bufferView":2,"componentType":5126,"count":2,"type":"VEC3"},
            {"bufferView":3,"componentType":5126,"count":2,"type":"VEC3"},
            {"bufferView":4,"componentType":5125,"count":3,"type":"SCALAR"}],
        "bufferViews":[
            {"buffer":0,"byteOffset":0,"byteLength":36},
            {"buffer":0,"byteOffset":36,"byteLength":8},
            {"buffer":0,"byteOffset":44,"byteLength":24},
            {"buffer":0,"byteOffset":68,"byteLength":24},
            {"buffer":0,"byteOffset":92,"byteLength":12}],
        "buffers":[{"byteLength":104}]}"#;
    let mut json = json.as_bytes().to_vec();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total = u32::try_from(12 + 8 + json.len() + 8 + bin.len()).expect("small glb");
    let mut glb = Vec::new();
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&total.to_le_bytes());
    glb.extend_from_slice(&u32::try_from(json.len()).expect("small json").to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json);
    glb.extend_from_slice(&u32::try_from(bin.len()).expect("small bin").to_le_bytes());
    glb.extend_from_slice(b"BIN\0");
    glb.extend_from_slice(&bin);
    glb
}

/// The weight and procgen panel actions on a fresh editor (ADR-8381).
///
/// `every_panel_action_runs_headless` reaches these ids only after every earlier panel action.
/// This test runs just the three new ids so a failure in an older action cannot hide them.
#[test]
fn edit_layer_weight_and_procgen_actions_run_headless() {
    let dir = project();
    let mut agent = Agent::new(&dir);
    for action in ACTIONS.iter().filter(|a| {
        matches!(
            a.id,
            "terrain.edit_layer_detach" | "terrain.edit_layer_attach" | "terrain.edit_layer_paint_weight"
        )
    }) {
        let method = match action.id {
            "terrain.edit_layer_paint_weight" => "terrain.edit_layer_paint_weights",
            _ => "terrain.edit_layer_procgen",
        };
        assert_eq!(action.path, ActionPath::Method(method));
        let terrain = agent.world().spawn(small_terrain()).id();
        let bits = terrain.to_bits();
        let created = agent.call(
            "terrain.edit_layer_create",
            json!({ "entity": bits, "name": "Hills", "blend": "add" }),
        );
        assert!(created["layers"][0]["procgen"].is_null(), "{created}");
        match action.id {
            "terrain.edit_layer_paint_weight" => {
                let painted = agent.call(
                    "terrain.edit_layer_paint_weights",
                    json!({ "entity": bits, "layer": "Hills", "col": 0, "row": 0, "channels": [0.0, 1.0] }),
                );
                let weights = painted["weights"].as_array().expect("weights");
                let sum: f64 = weights.iter().filter_map(serde_json::Value::as_f64).sum();
                assert!((sum - 1.0).abs() < 1e-5, "weights sum to 1: {painted}");
                assert!(
                    (weights[1].as_f64().unwrap_or(0.0) - 0.5).abs() < 1e-5,
                    "{painted}"
                );
                let err = agent
                    .try_call(
                        "terrain.edit_layer_paint_weights",
                        json!({
                            "entity": bits,
                            "layer": "Hills",
                            "col": 0,
                            "row": 0,
                            "channels": [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
                        }),
                    )
                    .expect_err("more than eight channels");
                assert_eq!(code(&err), "edit_layer_bad_weight", "{err:?}");
            }
            "terrain.edit_layer_attach" | "terrain.edit_layer_detach" => {
                let attached = agent.call(
                    "terrain.edit_layer_procgen",
                    json!({
                        "entity": bits,
                        "layer": "Hills",
                        "graph": "assets/hills.pgraph.ron",
                        "output": "height",
                    }),
                );
                assert_eq!(
                    attached["layers"][0]["procgen"]["graph"], "assets/hills.pgraph.ron",
                    "{attached}"
                );
                assert_eq!(attached["layers"][0]["procgen"]["output"], "height", "{attached}");
                let err = agent
                    .try_call(
                        "terrain.stroke",
                        json!({
                            "entity": bits,
                            "edit_layer": "Hills",
                            "mode": "raise",
                            "center": [0.0, 0.0],
                            "radius": 4.0,
                            "strength": 1.0,
                        }),
                    )
                    .expect_err("an owned layer refuses a brush");
                assert_eq!(code(&err), "edit_layer_owned", "{err:?}");
                if action.id == "terrain.edit_layer_detach" {
                    let detached = agent.call(
                        "terrain.edit_layer_procgen",
                        json!({ "entity": bits, "layer": "Hills", "detach": true }),
                    );
                    assert!(detached["layers"][0]["procgen"].is_null(), "{detached}");
                }
            }
            _ => {}
        }
        let _ = agent.world().despawn(terrain);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// P-310 (ADR-8224): Capture play window, Previous draw and Next draw run headless.
///
/// `every_panel_action_runs_headless` lists these arms, but that loop stops on a
/// pre-existing terrain opacity assertion (`edit_layer_bad_opacity` vs the contract
/// range's `F0002`, already at integ-v06l). This test runs only the new actions.
#[test]
fn frame_debugger_draw_and_play_window_actions_run_headless() {
    let dir = project();
    let mut agent = Agent::new(&dir);
    for id in [
        "frame_debugger.capture_play_window",
        "frame_debugger.previous_draw",
        "frame_debugger.next_draw",
    ] {
        let action = ACTIONS
            .iter()
            .find(|action| action.id == id)
            .unwrap_or_else(|| panic!("{id} is registered"));
        match id {
            "frame_debugger.capture_play_window" => {
                assert_eq!(action.path, ActionPath::Method("editor.frame_capture"));
                let err = agent
                    .try_call(
                        "editor.frame_capture",
                        json!({ "action": "start", "source": "play_window" }),
                    )
                    .expect_err("no play window");
                assert_eq!(code(&err), "capture_no_play_window", "{err:?}");
                let out = agent.call("editor.frame_capture", json!({ "action": "state" }));
                assert_ne!(out["state"], "pending", "{out}");
            }
            "frame_debugger.previous_draw" | "frame_debugger.next_draw" => {
                assert_eq!(action.path, ActionPath::Method("editor.frame_draw"));
                let step = if id == "frame_debugger.next_draw" {
                    "next"
                } else {
                    "previous"
                };
                let err = agent
                    .try_call("editor.frame_draw", json!({ "step": step }))
                    .expect_err("no capture to step through");
                assert_eq!(code(&err), "frame_capture_missing", "{err:?}");
            }
            _ => unreachable!(),
        }
    }
    drop(agent);
    let _ = std::fs::remove_dir_all(dir);
}
