// SPDX-License-Identifier: MPL-2.0
//! Named layouts, workspaces, maximize, the full-screen viewport, pane changes and feature profiles
//! through the headless shell and the agent methods (ADR-7753, parity rows P-002, P-008, P-010,
//! P-011, P-013, P-017, P-018).

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_editor::editor_layout::{self, LayoutDoc};
use regia_editor::editor_prefs::EditorConfigPaths;
use regia_editor::shell::{EditorShell, Panel};
use regia_editor::workspaces;
use serde_json::{Value, json};

fn unique_dir(label: &str) -> PathBuf {
    regia_test_utils::unique_temp_dir(&format!("workspaces-{label}"))
}

fn temp_project(label: &str) -> PathBuf {
    let dir = unique_dir(label);
    std::fs::create_dir_all(dir.join("assets/levels")).expect("assets");
    std::fs::write(
        dir.join("Regia.toml"),
        "[project]\nname = \"W\"\nid = \"com.example.w\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("assets/levels/main.level.ron"),
        "( format_version: 1, entities: [] )",
    )
    .expect("level");
    dir
}

fn edit_app(config: &Path, root: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<regia_editor::command::UndoStack>()
        .init_resource::<regia_editor::selection::Selection>()
        .init_resource::<regia_editor::viewport::EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>();
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());
    app.world_mut().insert_resource(EditorConfigPaths::at(config));
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
    app.update();
    app
}

/// Runs shell frames without a GPU; `events` go into the first one.
fn frames_with(
    shell: &mut EditorShell,
    ctx: &egui::Context,
    world: &mut World,
    frames: u32,
    events: Vec<egui::Event>,
) {
    let mut events = Some(events);
    for frame in 0..frames {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            time: Some(f64::from(frame) / 60.0),
            predicted_dt: 1.0 / 60.0,
            events: events.take().unwrap_or_default(),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| shell.ui(ui, world, None));
        output.textures_delta.clear();
    }
}

fn frames(shell: &mut EditorShell, ctx: &egui::Context, world: &mut World, frames: u32) {
    frames_with(shell, ctx, world, frames, Vec::new());
}

fn key(key: egui::Key, modifiers: egui::Modifiers) -> Vec<egui::Event> {
    vec![
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        },
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers,
        },
    ]
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, String> {
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params).map_err(|e| {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .map_or_else(|| e.message.clone(), str::to_owned)
    })
}

/// The shown layout's file text (the serialized tree).
fn tree(shell: &EditorShell) -> String {
    shell.layout().to_text()
}

#[test]
fn two_named_layouts_survive_a_restart_and_each_restores_its_tree() {
    let config = unique_dir("named-config");
    let root = temp_project("named");
    let mut app = edit_app(&config, &root);
    let ctx = egui::Context::default();
    let mut shell = EditorShell::new(None, Some(root.clone()));
    frames(&mut shell, &ctx, app.world_mut(), 2);
    assert_eq!(
        call(app.world_mut(), "editor.layout_list", json!({})).expect("list")["layouts"],
        json!([])
    );

    // Layout one: the built-in layout with Preferences opened.
    call(
        app.world_mut(),
        "editor.layout_apply",
        json!({ "open": "preferences" }),
    )
    .expect("open");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    let one = tree(&shell);
    call(
        app.world_mut(),
        "editor.layout_save_named",
        json!({ "name": "Prefs open" }),
    )
    .expect("save one");

    // Layout two: a custom split.
    let custom = json!({
        "format_version": 2,
        "root": { "Split": { "axis": "Vertical", "fraction": 0.4,
            "first": { "Tabs": { "tabs": ["viewport", "inspector"], "active": 1 } },
            "second": { "Tabs": { "tabs": ["content", "outliner"], "active": 0 } } } },
        "known_panels": editor_layout::BUILT_IN_PANELS.iter().map(|p| p.id()).collect::<Vec<_>>(),
    });
    call(
        app.world_mut(),
        "editor.layout_apply",
        json!({ "layout": custom }),
    )
    .expect("apply");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    let two = tree(&shell);
    assert_ne!(one, two);
    let saved = call(
        app.world_mut(),
        "editor.layout_save_named",
        json!({ "name": "Tall", "workspace": true }),
    )
    .expect("save two");
    assert_eq!(saved["saved"]["workspace"], true);
    // Each file is the layout shape, unchanged.
    let file = workspaces::named_layout_path(&workspaces::named_layout_dir(&config), "Tall", true);
    assert_eq!(std::fs::read_to_string(&file).expect("file"), two);

    // Restart: a new app and shell on the same config and project.
    drop(shell);
    let mut app = edit_app(&config, &root);
    let mut shell = EditorShell::new(None, Some(root.clone()));
    frames(&mut shell, &ctx, app.world_mut(), 2);
    let listed = call(app.world_mut(), "editor.layout_list", json!({})).expect("list");
    let names: Vec<&str> = listed["layouts"]
        .as_array()
        .expect("layouts")
        .iter()
        .filter_map(|l| l["name"].as_str())
        .collect();
    assert_eq!(names, ["Prefs open", "Tall"]);
    for (name, expected) in [("Prefs open", &one), ("tall", &two), ("Prefs open", &one)] {
        call(
            app.world_mut(),
            "editor.layout_apply_named",
            json!({ "name": name }),
        )
        .expect("apply named");
        frames(&mut shell, &ctx, app.world_mut(), 1);
        assert_eq!(&tree(&shell), expected, "{name}");
    }

    // Rename and delete.
    call(
        app.world_mut(),
        "editor.layout_save_named",
        json!({ "name": "Tall view", "rename_from": "Tall" }),
    )
    .expect("rename");
    assert_eq!(
        call(
            app.world_mut(),
            "editor.layout_apply_named",
            json!({ "name": "Tall" })
        )
        .expect_err("the call fails"),
        "layout_not_found"
    );
    assert_eq!(
        call(
            app.world_mut(),
            "editor.layout_save_named",
            json!({ "name": "bad/name" })
        )
        .expect_err("the call fails"),
        "bad_layout_name"
    );
    call(
        app.world_mut(),
        "editor.layout_delete",
        json!({ "name": "Prefs open" }),
    )
    .expect("delete");
    let listed = call(app.world_mut(), "editor.layout_list", json!({})).expect("list");
    assert_eq!(listed["layouts"].as_array().map(Vec::len), Some(1));
    let _ = std::fs::remove_dir_all(&config);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn switching_workspace_opens_its_panels_and_back_restores_the_tree_byte_equal() {
    let config = unique_dir("ws-config");
    let root = temp_project("ws");
    let mut app = edit_app(&config, &root);
    let ctx = egui::Context::default();
    let mut shell = EditorShell::new(None, Some(root.clone()));
    frames(&mut shell, &ctx, app.world_mut(), 2);
    // A changed Level layout, so "back" proves the parked tree and not the built-in one.
    call(
        app.world_mut(),
        "editor.layout_apply",
        json!({ "open": "preferences" }),
    )
    .expect("open");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    let level = tree(&shell);

    let listed = call(app.world_mut(), "editor.workspace_list", json!({})).expect("list");
    assert_eq!(listed["workspace"], "level");
    call(
        app.world_mut(),
        "editor.workspace_set",
        json!({ "id": "animation" }),
    )
    .expect("set");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    let doc = shell.layout();
    for id in ["anim_graph", "anim_clip", "anim_skeleton", "montage", "viewport"] {
        assert!(doc.panel_ids().contains(&id), "animation workspace lacks {id}");
    }
    let got = call(app.world_mut(), "editor.workspace_list", json!({})).expect("list");
    assert_eq!(got["workspace"], "animation");

    call(app.world_mut(), "editor.workspace_set", json!({ "id": "level" })).expect("back");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    assert_eq!(tree(&shell), level);
    assert_eq!(
        call(app.world_mut(), "editor.workspace_set", json!({ "id": "nope" })).expect_err("the call fails"),
        "unknown_workspace"
    );

    // The palette route runs the same switch.
    let ran = call(
        app.world_mut(),
        "editor.command",
        json!({ "run": "workspace.materials" }),
    )
    .expect("palette");
    assert_eq!(ran["ran"]["id"], "workspace.materials");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    assert_eq!(shell.views().workspace(), "materials");

    // A user workspace: a named layout with the workspace flag.
    call(
        app.world_mut(),
        "editor.layout_save_named",
        json!({ "name": "Mats", "workspace": true }),
    )
    .expect("save");
    let mats = tree(&shell);
    call(app.world_mut(), "editor.workspace_set", json!({ "id": "level" })).expect("level");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    call(
        app.world_mut(),
        "editor.workspace_set",
        json!({ "id": "user:Mats" }),
    )
    .expect("user");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    assert_eq!(tree(&shell), mats);
    let _ = std::fs::remove_dir_all(&config);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn every_built_in_workspace_opens_its_panels() {
    let config = unique_dir("ws-each-config");
    let root = temp_project("ws-each");
    let mut app = edit_app(&config, &root);
    let ctx = egui::Context::default();
    let mut shell = EditorShell::new(None, Some(root.clone()));
    frames(&mut shell, &ctx, app.world_mut(), 2);
    for workspace in workspaces::BUILT_IN_WORKSPACES {
        call(
            app.world_mut(),
            "editor.workspace_set",
            json!({ "id": workspace.id }),
        )
        .expect("set");
        frames(&mut shell, &ctx, app.world_mut(), 1);
        let doc = shell.layout();
        for panel in workspace.panels() {
            assert!(
                doc.panel_ids().contains(&panel.id()),
                "{} lacks {}",
                workspace.id,
                panel.id()
            );
        }
        if workspace.opens_profiler {
            assert!(
                app.world()
                    .get_resource::<regia_editor::profiler_panel::ProfilerPanel>()
                    .is_some_and(|p| p.open)
            );
        }
    }
    let _ = std::fs::remove_dir_all(&config);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn ctrl_space_maximizes_and_restores_and_f11_hides_the_chrome() {
    let config = unique_dir("max-config");
    let root = temp_project("max");
    let mut app = edit_app(&config, &root);
    let ctx = egui::Context::default();
    let mut shell = EditorShell::new(None, Some(root.clone()));
    frames(&mut shell, &ctx, app.world_mut(), 2);
    let before = tree(&shell);

    // The keyboard path: Ctrl+Space with no pointer maximizes the focused tab.
    frames_with(
        &mut shell,
        &ctx,
        app.world_mut(),
        2,
        key(egui::Key::Space, egui::Modifiers::COMMAND),
    );
    assert!(
        shell.views().maximized().is_some(),
        "Ctrl+Space maximized nothing"
    );
    // The saved layout is still the whole dock.
    assert_eq!(tree(&shell), before);
    frames_with(
        &mut shell,
        &ctx,
        app.world_mut(),
        2,
        key(egui::Key::Space, egui::Modifiers::COMMAND),
    );
    assert_eq!(shell.views().maximized(), None);
    assert_eq!(tree(&shell), before);

    // The agent route, by panel.
    call(
        app.world_mut(),
        "editor.maximize_panel",
        json!({ "panel": "content" }),
    )
    .expect("max");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    assert_eq!(shell.views().maximized(), Some(Panel::Content));
    let status = call(app.world_mut(), "editor.workspace_list", json!({})).expect("status");
    assert_eq!(status["maximized"], "content");
    call(app.world_mut(), "editor.maximize_panel", json!({})).expect("restore");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    assert_eq!(tree(&shell), before);
    assert_eq!(
        call(
            app.world_mut(),
            "editor.maximize_panel",
            json!({ "panel": "nope" })
        )
        .expect_err("the call fails"),
        "unknown_panel"
    );

    // F11: the viewport alone, the chrome hidden; F11 again restores.
    frames_with(
        &mut shell,
        &ctx,
        app.world_mut(),
        2,
        key(egui::Key::F11, egui::Modifiers::NONE),
    );
    assert!(shell.views().chrome_hidden());
    assert!(workspaces::fullscreen_in(app.world()));
    frames_with(
        &mut shell,
        &ctx,
        app.world_mut(),
        2,
        key(egui::Key::F11, egui::Modifiers::NONE),
    );
    assert!(!shell.views().chrome_hidden());
    assert_eq!(tree(&shell), before);

    // Escape's request (what the window host queues when no play runs) leaves full screen.
    call(
        app.world_mut(),
        "editor.maximize_panel",
        json!({ "fullscreen": true }),
    )
    .expect("fs");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    assert!(shell.views().chrome_hidden());
    workspaces::ViewRequests::push(app.world_mut(), workspaces::ViewRequest::LeaveFullscreen);
    frames(&mut shell, &ctx, app.world_mut(), 1);
    assert!(!shell.views().chrome_hidden());
    assert_eq!(tree(&shell), before);
    let _ = std::fs::remove_dir_all(&config);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn add_and_replace_from_the_tab_controls_equal_the_agent_op() {
    let config = unique_dir("pane-config");
    let root = temp_project("pane");
    let ctx_a = egui::Context::default();
    let ctx_b = egui::Context::default();
    let mut gui = edit_app(&config, &root);
    let mut agent = edit_app(&config, &root);
    let mut shell_a = EditorShell::new(None, Some(root.clone()));
    let mut shell_b = EditorShell::new(None, Some(root.clone()));
    frames(&mut shell_a, &ctx_a, gui.world_mut(), 2);
    frames(&mut shell_b, &ctx_b, agent.world_mut(), 2);

    // The add popup's pick and the tab menu's Replace with.
    workspaces::pick_add(gui.world_mut(), Panel::Preferences, Some(Panel::Inspector));
    frames(&mut shell_a, &ctx_a, gui.world_mut(), 1);
    workspaces::pick_replace(gui.world_mut(), Panel::Outliner, Panel::Layers);
    frames(&mut shell_a, &ctx_a, gui.world_mut(), 1);

    call(
        agent.world_mut(),
        "editor.panel_replace",
        json!({ "to": "preferences", "near": "inspector" }),
    )
    .expect("add");
    frames(&mut shell_b, &ctx_b, agent.world_mut(), 1);
    call(
        agent.world_mut(),
        "editor.panel_replace",
        json!({ "from": "outliner", "to": "layers" }),
    )
    .expect("replace");
    frames(&mut shell_b, &ctx_b, agent.world_mut(), 1);

    assert_eq!(tree(&shell_a), tree(&shell_b));
    let doc: LayoutDoc = shell_b.layout();
    assert!(!doc.panel_ids().contains(&"outliner"));
    assert_eq!(doc.panel_ids().iter().filter(|id| **id == "layers").count(), 1);
    assert_eq!(
        call(
            agent.world_mut(),
            "editor.panel_replace",
            json!({ "from": "outliner", "to": "kit" })
        )
        .expect_err("the call fails"),
        "panel_not_open"
    );
    assert_eq!(
        call(
            agent.world_mut(),
            "editor.panel_replace",
            json!({ "from": "viewport", "near": "content", "to": "kit" })
        )
        .expect_err("the call fails"),
        "bad_pane_request"
    );
    let _ = std::fs::remove_dir_all(&config);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn ui_only_profile_hides_panels_from_view_palette_and_create_and_full_brings_them_back() {
    let config = unique_dir("profile-config");
    let root = temp_project("profile");
    let mut app = edit_app(&config, &root);
    let ctx = egui::Context::default();
    let mut shell = EditorShell::new(None, Some(root.clone()));
    frames(&mut shell, &ctx, app.world_mut(), 2);
    assert!(shell.layout().panel_ids().contains(&"anim_graph"));
    let palette_ids = |world: &mut World| -> Vec<String> {
        let out = call(world, "editor.command", json!({ "query": "", "limit": 200 })).expect("palette");
        out["commands"]
            .as_array()
            .expect("commands")
            .iter()
            .filter_map(|c| c["id"].as_str().map(str::to_owned))
            .collect()
    };
    assert!(palette_ids(app.world_mut()).contains(&"window.open_proc_graph".to_owned()));

    let set = call(
        app.world_mut(),
        "editor.feature_profile_set",
        json!({ "name": "UI only" }),
    )
    .expect("set");
    assert_eq!(set["active"]["name"], "UI only");
    assert!(
        root.join(regia_editor::feature_profiles::PROJECT_PROFILE_FILE)
            .is_file()
    );
    frames(&mut shell, &ctx, app.world_mut(), 1);
    let doc = shell.layout();
    assert!(
        !doc.panel_ids().contains(&"anim_graph"),
        "a hidden panel kept its tab"
    );
    assert!(doc.panel_ids().contains(&"ui_designer"));
    assert!(regia_editor::feature_profiles::panel_hidden(
        app.world(),
        Panel::AnimGraph
    ));
    assert!(!regia_editor::feature_profiles::panel_hidden(
        app.world(),
        Panel::UiDesigner
    ));
    let ids = palette_ids(app.world_mut());
    assert!(!ids.contains(&"window.open_proc_graph".to_owned()));
    assert!(!ids.iter().any(|id| id.starts_with("create.")));
    assert!(ids.contains(&"window.open_ui_designer".to_owned()));
    assert!(regia_editor::feature_profiles::kind_hidden(app.world(), "cuboid"));
    // The profile is per project: a new session of this project starts with it.
    let mut again = edit_app(&config, &root);
    let got = call(again.world_mut(), "editor.feature_profile_get", json!({})).expect("get");
    assert_eq!(got["active"]["name"], "UI only");

    call(
        app.world_mut(),
        "editor.feature_profile_set",
        json!({ "name": "Full" }),
    )
    .expect("full");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    assert!(shell.layout().panel_ids().contains(&"anim_graph"));
    assert!(palette_ids(app.world_mut()).contains(&"window.open_proc_graph".to_owned()));
    assert!(!regia_editor::feature_profiles::kind_hidden(
        app.world(),
        "cuboid"
    ));
    assert!(
        !root
            .join(regia_editor::feature_profiles::PROJECT_PROFILE_FILE)
            .exists()
    );

    // A custom profile saved to share, then refused input.
    let saved = call(
        app.world_mut(),
        "editor.feature_profile_set",
        json!({ "profile": { "name": "No audio", "hidden_areas": ["audio"] }, "save_as": "No audio" }),
    )
    .expect("custom");
    assert!(saved["saved_to"].as_str().is_some_and(|p| Path::new(p).is_file()));
    assert_eq!(saved["hidden_panels"], json!(["audio_mixer", "cue_editor"]));
    assert_eq!(
        call(
            app.world_mut(),
            "editor.feature_profile_set",
            json!({ "name": "nope" })
        )
        .expect_err("the call fails"),
        "unknown_profile"
    );
    assert_eq!(
        call(
            app.world_mut(),
            "editor.feature_profile_set",
            json!({ "profile": { "name": "x", "hidden_areas": ["4d"] } })
        )
        .expect_err("the call fails"),
        "profile_parse"
    );
    let _ = std::fs::remove_dir_all(&config);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn window_state_level_and_asset_documents_come_back_for_the_same_project() {
    use regia_editor::window_state::{self, MonitorInfo, ObservedWindow};
    let config = unique_dir("window-config");
    let root = temp_project("window");
    let mut app = edit_app(&config, &root);
    let ctx = egui::Context::default();
    let mut shell = EditorShell::new(None, Some(root.clone()));
    frames(&mut shell, &ctx, app.world_mut(), 2);
    // A fake window state from the host, a saved level open, and an animation graph open.
    app.world_mut().insert_resource(ObservedWindow {
        monitor: Some("DP-9".to_owned()),
        position: Some((10, 20)),
        size: (1440, 900),
        fullscreen: true,
        maximized: false,
    });
    let level = root.join("assets/levels/main.level.ron");
    let open = regia_editor::level_io::open_level_file(app.world_mut(), &level).expect("open level");
    app.world_mut().insert_resource(open);
    let graph = root.join("assets/anim/walk.animgraph.ron");
    std::fs::create_dir_all(graph.parent().expect("dir")).expect("anim dir");
    std::fs::write(&graph, "()").expect("graph");
    app.world_mut()
        .get_resource_or_insert_with(regia_editor::anim::AnimEditor::default)
        .graph_path = Some(graph.clone());
    shell.save_layout_on_exit(app.world());

    let record = window_state::read_record(&config, &root).expect("record");
    assert_eq!(record.size, (1440, 900));
    assert_eq!(record.position, Some((10, 20)));
    assert!(record.fullscreen);
    assert_eq!(record.level.as_deref(), Some("assets/levels/main.level.ron"));
    assert_eq!(record.asset_tabs, ["assets/anim/walk.animgraph.ron"]);
    // Another project has no record.
    assert_eq!(window_state::read_record(&config, &unique_dir("other")), None);

    // The host places the window: DP-9 is gone, so the primary display, centered.
    let monitors = [
        MonitorInfo {
            name: Some("HDMI-1".to_owned()),
            position: (0, 0),
            size: (1920, 1080),
            primary: false,
        },
        MonitorInfo {
            name: Some("eDP-1".to_owned()),
            position: (1920, 0),
            size: (2560, 1440),
            primary: true,
        },
    ];
    let placement = window_state::place(&record, &monitors);
    assert_eq!(placement.monitor, Some(1));
    assert_eq!(
        placement.position,
        Some((1920 + (2560 - 1440) / 2, (1440 - 900) / 2))
    );
    assert!(placement.fullscreen);

    // The next start of the project reopens the level and queues the asset document.
    let mut fresh = edit_app(&config, &root);
    let line = window_state::reopen_documents(fresh.world_mut(), &config, &root).expect("reopened");
    assert!(line.contains("reopening 1 asset document"), "{line}");
    assert_eq!(
        fresh.world().resource::<window_state::ReopenQueue>().pending,
        std::slice::from_ref(&graph)
    );
    let opened = fresh.world().resource::<regia_editor::level_io::OpenLevel>();
    assert_eq!(opened.path.as_deref(), Some(level.as_path()));

    // The shell does the same on its first frame and drains the queue one document per frame.
    let mut next = edit_app(&config, &root);
    let mut shell = EditorShell::new(None, Some(root.clone()));
    frames(&mut shell, &ctx, next.world_mut(), 2);
    assert_eq!(
        next.world()
            .resource::<regia_editor::level_io::OpenLevel>()
            .path
            .as_deref(),
        Some(level.as_path())
    );
    assert!(
        next.world()
            .get_resource::<window_state::ReopenQueue>()
            .is_some_and(|q| q.pending.is_empty())
    );
    let _ = std::fs::remove_dir_all(&config);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn opening_a_panel_while_another_is_maximized_restores_the_dock_first() {
    let config = unique_dir("max-open-config");
    let root = temp_project("max-open");
    let mut app = edit_app(&config, &root);
    let ctx = egui::Context::default();
    let mut shell = EditorShell::new(None, Some(root.clone()));
    frames(&mut shell, &ctx, app.world_mut(), 2);
    call(
        app.world_mut(),
        "editor.maximize_panel",
        json!({ "panel": "content" }),
    )
    .expect("max");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    assert_eq!(shell.views().maximized(), Some(Panel::Content));
    call(
        app.world_mut(),
        "editor.layout_apply",
        json!({ "open": "preferences" }),
    )
    .expect("open");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    assert_eq!(shell.views().maximized(), None);
    let doc = shell.layout();
    assert!(doc.panel_ids().contains(&"preferences"));
    assert!(doc.panel_ids().contains(&"viewport"), "the whole dock came back");
    let _ = std::fs::remove_dir_all(&config);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_terrain_workspace_turns_on_the_terrain_tools_and_back() {
    let config = unique_dir("terrain-mode-config");
    let root = temp_project("terrain-mode");
    let mut app = edit_app(&config, &root);
    let ctx = egui::Context::default();
    let mut shell = EditorShell::new(None, Some(root.clone()));
    frames(&mut shell, &ctx, app.world_mut(), 2);
    call(app.world_mut(), "editor.workspace_set", json!({"id": "terrain"})).expect("terrain workspace");
    frames(&mut shell, &ctx, app.world_mut(), 2);
    let state = call(app.world_mut(), "editor.workspace_list", json!({})).expect("state");
    assert_eq!(state["mode"], "terrain");
    assert!(
        app.world()
            .resource::<regia_editor::terrain_tool::TerrainTool>()
            .active
    );
    assert!(!regia_editor::mode2d::is_active(app.world()));
    assert!(shell.layout().panel_ids().contains(&"terrain"));
    call(app.world_mut(), "editor.workspace_set", json!({"id": "level"})).expect("level workspace");
    frames(&mut shell, &ctx, app.world_mut(), 2);
    let state = call(app.world_mut(), "editor.workspace_list", json!({})).expect("state");
    assert_eq!(state["mode"], "world");
    assert!(
        !app.world()
            .resource::<regia_editor::terrain_tool::TerrainTool>()
            .active
    );
    let _ = std::fs::remove_dir_all(config);
    let _ = std::fs::remove_dir_all(root);
}

fn two_area_app() -> (App, workspaces::DockViews, egui_dock::DockState<Panel>) {
    let mut app = App::new();
    regia_editor::agent_ext::install(app.world_mut());
    let mut dock = egui_dock::DockState::new(vec![Panel::Viewport, Panel::Content]);
    dock.main_surface_mut().split_right(
        egui_dock::NodeIndex::root(),
        0.6,
        vec![Panel::Inspector, Panel::Outliner],
    );
    let doc = LayoutDoc::from_dock(&dock);
    app.world_mut().insert_resource(editor_layout::LayoutSnapshot {
        doc,
        fingerprint: editor_layout::dock_fingerprint(&dock),
        source: editor_layout::LayoutSource::Default,
        report: Default::default(),
    });
    (app, workspaces::DockViews::default(), dock)
}

#[test]
fn join_merges_two_areas_into_one_stack() {
    let (mut app, mut views, mut dock) = two_area_app();
    call(
        app.world_mut(),
        "editor.layout_join",
        json!({"from": "inspector", "into": "viewport"}),
    )
    .expect("join");
    views.apply_requests(&mut dock, app.world_mut());
    let groups = workspaces::DockGroups::of(&dock);
    assert_eq!(groups.groups.len(), 1);
    assert_eq!(
        groups.groups.values().next().expect("one leaf"),
        &vec![Panel::Viewport, Panel::Content, Panel::Inspector, Panel::Outliner]
    );
}

#[test]
fn swap_exchanges_two_panes() {
    let (mut app, mut views, mut dock) = two_area_app();
    let before = workspaces::DockGroups::of(&dock);
    call(
        app.world_mut(),
        "editor.layout_swap",
        json!({"from": "viewport", "with": "inspector"}),
    )
    .expect("swap");
    views.apply_requests(&mut dock, app.world_mut());
    let after = workspaces::DockGroups::of(&dock);
    let keys: Vec<_> = before.groups.keys().copied().collect();
    assert_eq!(after.groups[&keys[0]], before.groups[&keys[1]]);
    assert_eq!(after.groups[&keys[1]], before.groups[&keys[0]]);
}

#[test]
fn add_tab_has_an_accessible_name_and_no_unnamed_widgets() {
    struct Viewer;
    impl egui_dock::TabViewer for Viewer {
        type Tab = Panel;
        fn id(&mut self, tab: &mut Panel) -> egui::Id {
            egui::Id::new(tab.id())
        }
        fn title(&mut self, tab: &mut Panel) -> egui::WidgetText {
            tab.title().into()
        }
        fn ui(&mut self, _ui: &mut egui::Ui, _tab: &mut Panel) {}
    }
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut world = World::new();
    let mut dock = egui_dock::DockState::new(vec![Panel::Viewport]);
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        let mut style = egui_dock::Style::from_egui(ui.style());
        workspaces::reserve_overflow_button(&mut style);
        let height = style.tab_bar.height;
        egui_dock::DockArea::new(&mut dock)
            .style(style)
            .show_inside(ui, &mut Viewer);
        workspaces::tab_overflow_buttons(ui, &mut world, &dock, height);
    });
    output.textures_delta.clear();
    let update = output.platform_output.accesskit_update.expect("accesskit");
    let names = regia_editor::a11y_audit::widget_names(&update);
    assert!(names.iter().any(|(_, name)| name == "Add panel"), "{names:?}");
    assert!(
        regia_editor::a11y_audit::unnamed_widgets(&update).is_empty(),
        "{names:?}"
    );
}

#[test]
fn invalid_area_operations_are_atomic() {
    let (mut app, mut views, mut dock) = two_area_app();
    let before = LayoutDoc::from_dock(&dock);
    for (method, params) in [
        (
            "editor.layout_join",
            json!({"from": "viewport", "into": "content"}),
        ),
        (
            "editor.layout_swap",
            json!({"from": "viewport", "with": "content"}),
        ),
        ("editor.layout_join", json!({"from": "viewport"})),
        ("editor.layout_swap", json!({"from": "viewport", "with": 5})),
    ] {
        assert!(call(app.world_mut(), method, params).is_err(), "{method}");
        views.apply_requests(&mut dock, app.world_mut());
        assert_eq!(LayoutDoc::from_dock(&dock), before);
    }
}

#[test]
fn a_workspace_mode_override_survives_switching_and_rejects_bad_modes() {
    let (mut app, mut views, mut dock) = two_area_app();
    call(
        app.world_mut(),
        "editor.workspace_set",
        json!({"id": "world", "mode": "tiles2d"}),
    )
    .expect("mode");
    views.apply_requests(&mut dock, app.world_mut());
    views.publish(app.world_mut());
    assert!(regia_editor::mode2d::is_active(app.world()));
    for id in ["level", "world"] {
        call(app.world_mut(), "editor.workspace_set", json!({"id": id})).expect("switch");
        views.apply_requests(&mut dock, app.world_mut());
        views.publish(app.world_mut());
    }
    assert!(regia_editor::mode2d::is_active(app.world()));
    let before = LayoutDoc::from_dock(&dock);
    for mode in [json!("bad"), json!(42), json!(null)] {
        assert_eq!(
            call(
                app.world_mut(),
                "editor.workspace_set",
                json!({"id": "world", "mode": mode})
            )
            .expect_err("invalid mode"),
            "workspace_mode"
        );
    }
    views.apply_requests(&mut dock, app.world_mut());
    assert_eq!(LayoutDoc::from_dock(&dock), before);
}

#[test]
fn area_addresses_can_join_an_extension_only_pane() {
    let (mut app, mut views, _) = two_area_app();
    let custom = Panel::ExtensionPanel("test.tools", "Test tools");
    let mut dock = egui_dock::DockState::new(vec![custom]);
    dock.main_surface_mut()
        .split_right(egui_dock::NodeIndex::root(), 0.5, vec![Panel::Viewport]);
    workspaces::DockGroups::refresh(app.world_mut(), &dock);
    call(
        app.world_mut(),
        "editor.layout_join",
        json!({"from": "area:0:1", "into": "viewport"}),
    )
    .expect("join extension area");
    views.apply_requests(&mut dock, app.world_mut());
    assert_eq!(dock.iter_leaves().count(), 1);
    assert_eq!(
        dock.iter_all_tabs().map(|(_, tab)| *tab).collect::<Vec<_>>(),
        vec![Panel::Viewport, custom]
    );
}
