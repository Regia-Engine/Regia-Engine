// SPDX-License-Identifier: MPL-2.0
//! Floated panels as OS windows (ADR-7821, parity rows P-001, P-012, P-015) through the headless
//! shell, the agent methods and egui's multi-viewport output. The helper [`frame`] does what the
//! editor binary's runner does each frame: the root pass, the frame effects, then one pass per
//! child viewport the root pass's `viewport_output` lists, each calling `os_window_ui`.

use std::path::{Path, PathBuf};

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_editor::editor_prefs::EditorConfigPaths;
use regia_editor::os_windows::{self, OsWindowStatus, OsWindowSupport};
use regia_editor::shell::{EditorShell, Panel};
use serde_json::{Value, json};

fn unique_dir(label: &str) -> PathBuf {
    regia_test_utils::unique_temp_dir(&format!("os-windows-{label}"))
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

/// The main window's inner rectangle on the desktop (points).
const MAIN: egui::Rect = egui::Rect {
    min: egui::pos2(100.0, 50.0),
    max: egui::pos2(1380.0, 850.0),
};

/// One frame's outputs: the root pass and each OS window's pass.
struct Frame {
    root: egui::FullOutput,
    windows: Vec<(egui::ViewportId, egui::FullOutput)>,
}

impl Frame {
    /// The child viewports egui listed (what the runner opens windows for).
    fn listed(&self) -> Vec<(egui::ViewportId, egui::ViewportBuilder)> {
        let mut out: Vec<_> = self
            .root
            .viewport_output
            .iter()
            .filter(|(id, _)| **id != egui::ViewportId::ROOT)
            .map(|(id, v)| (*id, v.builder.clone()))
            .collect();
        out.sort_by_key(|(id, _)| egui::Id::from(*id).value());
        out
    }
}

/// One editor frame the way the editor binary runs it (see the module docs).
fn frame(
    shell: &mut EditorShell,
    ctx: &egui::Context,
    world: &mut World,
    n: u32,
    events: Vec<egui::Event>,
) -> Frame {
    let mut root_raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, MAIN.size())),
        time: Some(f64::from(n) / 60.0),
        predicted_dt: 1.0 / 60.0,
        events,
        ..Default::default()
    };
    root_raw.viewports.insert(
        egui::ViewportId::ROOT,
        egui::ViewportInfo {
            inner_rect: Some(MAIN),
            ..Default::default()
        },
    );
    let mut root = ctx.run_ui(root_raw, |ui| shell.ui(ui, world, None));
    shell.frame_effects(ctx, world);
    root.textures_delta.clear();
    let children: Vec<egui::ViewportId> = root
        .viewport_output
        .keys()
        .copied()
        .filter(|id| *id != egui::ViewportId::ROOT)
        .collect();
    let mut windows = Vec::new();
    for id in children {
        let size = egui::vec2(480.0, 360.0);
        let mut raw = egui::RawInput {
            viewport_id: id,
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            time: Some(f64::from(n) / 60.0),
            predicted_dt: 1.0 / 60.0,
            ..Default::default()
        };
        raw.viewports.insert(
            id,
            egui::ViewportInfo {
                parent: Some(egui::ViewportId::ROOT),
                inner_rect: Some(egui::Rect::from_min_size(egui::pos2(1500.0, 120.0), size)),
                ..Default::default()
            },
        );
        let mut out = ctx.run_ui(raw, |ui| shell.os_window_ui(ui, id, world, None));
        out.textures_delta.clear();
        windows.push((id, out));
    }
    Frame { root, windows }
}

/// Frame numbers keep growing across calls (egui's time never goes back).
static FRAME: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn frames(shell: &mut EditorShell, ctx: &egui::Context, world: &mut World, count: u32) -> Frame {
    let next = || FRAME.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut last = frame(shell, ctx, world, next(), Vec::new());
    for _ in 1..count {
        last = frame(shell, ctx, world, next(), Vec::new());
    }
    last
}

/// Whether `shapes` hold a text run equal to `needle` (a tab title).
fn has_text(shapes: &[egui::epaint::ClippedShape], needle: &str) -> bool {
    text_count(shapes, needle) > 0
}

/// How many text runs of `shapes` equal `needle`. The workspace mode toolbar draws a button
/// for each panel of the mode (the Inspector among them) whether or not the panel floats, so
/// "the main window no longer draws the tab" is one run fewer than with the panel docked.
fn text_count(shapes: &[egui::epaint::ClippedShape], needle: &str) -> usize {
    fn walk(shape: &egui::Shape, needle: &str) -> usize {
        match shape {
            egui::Shape::Text(text) => usize::from(text.galley.text() == needle),
            egui::Shape::Vec(shapes) => shapes.iter().map(|s| walk(s, needle)).sum(),
            _ => 0,
        }
    }
    shapes.iter().map(|clipped| walk(&clipped.shape, needle)).sum()
}

/// A shell on a project, egui set up as the editor binary sets it (multi-viewport runner).
fn os_setup(label: &str) -> (App, EditorShell, egui::Context, PathBuf, PathBuf) {
    let config = unique_dir(&format!("{label}-config"));
    let root = temp_project(label);
    let mut app = edit_app(&config, &root);
    app.world_mut().insert_resource(OsWindowSupport::default());
    let ctx = egui::Context::default();
    ctx.set_embed_viewports(false);
    let shell = EditorShell::new(None, Some(root.clone()));
    (app, shell, ctx, config, root)
}

fn status(world: &World) -> OsWindowStatus {
    world
        .get_resource::<OsWindowStatus>()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn a_floated_inspector_is_its_own_os_window_listed_by_egui_and_docks_back() {
    let (mut app, mut shell, ctx, config, root) = os_setup("float");
    let world = app.world_mut();
    let first = frames(&mut shell, &ctx, world, 3);
    assert!(first.listed().is_empty(), "nothing floats yet");
    assert!(
        has_text(&first.root.shapes, "Inspector"),
        "the main window draws the Inspector tab"
    );
    let docked_runs = text_count(&first.root.shapes, "Inspector");

    let reply = call(
        world,
        "editor.panel_float",
        json!({ "panel": "inspector", "os_window": true }),
    )
    .expect("float");
    assert_eq!(reply["queued"]["action"], "float");
    assert_eq!(reply["os_window"], true, "{reply}");
    // The request lands on the next frame; the window is announced from the frame after.
    let after = frames(&mut shell, &ctx, world, 3);
    let listed = after.listed();
    assert_eq!(listed.len(), 1, "one child viewport: {listed:?}");
    let (id, builder) = &listed[0];
    assert!(
        matches!(
            after.root.viewport_output[id].class,
            egui::ViewportClass::Deferred
        ),
        "a deferred viewport the runner draws"
    );
    assert_eq!(after.root.viewport_output[id].parent, egui::ViewportId::ROOT);
    let title = builder.title.clone().unwrap_or_default();
    assert!(title.starts_with("Inspector"), "{title}");
    assert_eq!(builder.app_id.as_deref(), Some("regia.editor-panel"));
    // Opened beside the main window: the position is on the desktop, relative to the main window.
    let at = builder.position.expect("a position (the main window's is known)");
    assert!(at.x >= MAIN.min.x && at.y >= MAIN.min.y, "{at:?}");
    // The Inspector draws in its window's pass only.
    assert_eq!(after.windows.len(), 1);
    assert!(
        has_text(&after.windows[0].1.shapes, "Inspector"),
        "the OS window draws the Inspector"
    );
    assert_eq!(
        text_count(&after.root.shapes, "Inspector"),
        docked_runs - 1,
        "the main window no longer draws its tab"
    );
    let area = shell.panel_area(Panel::Inspector).expect("drawn");
    assert!(area.floating);

    // The agent sees it in its own OS window.
    let instances = call(world, "editor.panel_instances", json!({})).expect("instances");
    let row = instances["panels"]
        .as_array()
        .expect("panels")
        .iter()
        .find(|p| p["panel"] == "inspector")
        .cloned()
        .expect("inspector row");
    assert_eq!(row["os_window"], true, "{instances}");
    assert_eq!(row["floating"], true);
    assert_eq!(row["instance"], 0);
    let outliner = instances["panels"]
        .as_array()
        .expect("panels")
        .iter()
        .find(|p| p["panel"] == "outliner")
        .cloned()
        .expect("outliner row");
    assert_eq!(outliner["os_window"], false);
    assert_eq!(instances["os_mode"], true);
    let serial = status(world).window_of(Panel::Inspector).expect("floated").serial;
    assert_eq!(os_windows::viewport_id(serial), *id);

    // The layout saves the window (layout v2 `floating`), relative to the main window.
    let doc = shell.layout();
    assert_eq!(doc.floating.len(), 1, "{:?}", doc.floating);
    assert_eq!(doc.floating[0].tabs, ["inspector"]);
    let (x, y, w, h) = doc.floating[0].rect.expect("the OS window's rectangle");
    assert_eq!((x, y), (1500.0 - MAIN.min.x, 120.0 - MAIN.min.y));
    assert_eq!((w, h), (480.0, 360.0));

    // Dock back.
    call(
        world,
        "editor.panel_float",
        json!({ "panel": "inspector", "dock": true }),
    )
    .expect("dock");
    let docked = frames(&mut shell, &ctx, world, 3);
    assert!(
        docked.listed().is_empty(),
        "the window is gone: {:?}",
        docked.listed()
    );
    assert_eq!(
        text_count(&docked.root.shapes, "Inspector"),
        docked_runs,
        "the tab is back in the main window"
    );
    assert!(!shell.panel_area(Panel::Inspector).expect("drawn").floating);

    // Errors.
    assert_eq!(
        call(world, "editor.panel_float", json!({ "panel": "nope" })),
        Err("unknown_panel".into())
    );
    assert_eq!(
        call(
            world,
            "editor.panel_float",
            json!({ "panel": "inspector", "os_window": false })
        ),
        Err("float_mode_mismatch".into())
    );
    let _ = std::fs::remove_dir_all(&config);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_close_button_and_the_palette_dock_panels_back_and_the_palette_floats_the_focused_one() {
    let (mut app, mut shell, ctx, config, root) = os_setup("close");
    let world = app.world_mut();
    frames(&mut shell, &ctx, world, 2);
    // The keyboard path: the palette command (Ctrl+Shift+P, then Enter) floats the focused tab.
    regia_editor::workspaces::ViewRequests::push(
        world,
        regia_editor::workspaces::ViewRequest::Focus(Panel::Outliner),
    );
    frames(&mut shell, &ctx, world, 2);
    let ran = call(
        world,
        "editor.command",
        json!({ "run": os_windows::FLOAT_COMMAND }),
    )
    .expect("palette");
    assert!(ran.to_string().contains("Float"), "{ran}");
    let after = frames(&mut shell, &ctx, world, 3);
    assert_eq!(after.listed().len(), 1, "the focused Outliner floats");
    assert!(
        status(world)
            .window_of(Panel::Outliner)
            .is_some_and(|w| w.os_window)
    );
    // A second window: the Inspector, through the tab menu's request.
    os_windows::OsWindowRequests::push(world, os_windows::OsWindowRequest::Float(Panel::Inspector));
    let two = frames(&mut shell, &ctx, world, 3);
    assert_eq!(two.listed().len(), 2);
    assert_eq!(two.windows.len(), 2, "one pass per window");
    // The window's close button (the runner queues DockWindow) docks its panels back.
    let serial = status(world).window_of(Panel::Inspector).expect("floated").serial;
    os_windows::OsWindowRequests::push(world, os_windows::OsWindowRequest::DockWindow(serial));
    let one = frames(&mut shell, &ctx, world, 3);
    assert_eq!(one.listed().len(), 1);
    assert!(
        !shell.panel_area(Panel::Inspector).expect("open").floating,
        "docked, not closed"
    );
    // The palette docks every floated panel back.
    call(
        world,
        "editor.command",
        json!({ "run": os_windows::DOCK_ALL_COMMAND }),
    )
    .expect("palette");
    let none = frames(&mut shell, &ctx, world, 3);
    assert!(none.listed().is_empty());
    assert!(status(world).windows.is_empty());
    let _ = std::fs::remove_dir_all(&config);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn single_window_mode_and_a_runner_without_windows_keep_floats_inside_the_main_window() {
    let (mut app, mut shell, ctx, config, root) = os_setup("single");
    let world = app.world_mut();
    let start = frames(&mut shell, &ctx, world, 2);
    let docked_runs = text_count(&start.root.shapes, "Inspector");
    assert!(docked_runs > 0, "the main window draws the Inspector tab");
    call(
        world,
        "editor.prefs_set",
        json!({ "values": { "ui.floating_windows": "in_window" } }),
    )
    .expect("pref");
    frames(&mut shell, &ctx, world, 2);
    call(world, "editor.panel_float", json!({ "panel": "inspector" })).expect("float");
    let inside = frames(&mut shell, &ctx, world, 3);
    assert!(
        inside.listed().is_empty(),
        "single-window mode opens no OS window"
    );
    assert!(
        has_text(&inside.root.shapes, "Inspector"),
        "an egui window in the main window"
    );
    assert!(shell.panel_area(Panel::Inspector).expect("drawn").floating);
    let st = status(world);
    assert!(!st.os_mode);
    assert!(!st.windows[0].os_window);
    assert_eq!(
        call(
            world,
            "editor.panel_float",
            json!({ "panel": "outliner", "os_window": true })
        ),
        Err("float_mode_mismatch".into())
    );
    // Back to os: the same float becomes an OS window, nothing else changes.
    call(
        world,
        "editor.prefs_set",
        json!({ "values": { "ui.floating_windows": "os" } }),
    )
    .expect("pref");
    let os = frames(&mut shell, &ctx, world, 3);
    assert_eq!(os.listed().len(), 1);
    assert_eq!(
        text_count(&os.root.shapes, "Inspector"),
        docked_runs - 1,
        "the OS window, not the main window, draws the tab"
    );

    // A context whose runner embeds viewports (the headless editor): floats stay inside.
    let embedded = egui::Context::default();
    let mut shell2 = EditorShell::new(None, Some(root.clone()));
    let out = frames(&mut shell2, &embedded, world, 3);
    assert!(out.listed().is_empty());
    assert!(!status(world).os_mode);
    let _ = std::fs::remove_dir_all(&config);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_restored_layout_reopens_os_windows_where_they_were() {
    let (mut app, mut shell, ctx, config, root) = os_setup("restore");
    let world = app.world_mut();
    frames(&mut shell, &ctx, world, 2);
    call(world, "editor.panel_float", json!({ "panel": "material_graph" })).ok();
    // The material graph may not be open in the default layout: float the Content browser too.
    call(world, "editor.panel_float", json!({ "panel": "content" })).expect("float");
    frames(&mut shell, &ctx, world, 3);
    let doc = shell.layout();
    assert!(!doc.floating.is_empty());
    let text = doc.to_text();
    // A new session restores the layout file: the windows come back as OS windows at their place.
    let restored = regia_editor::editor_layout::LayoutDoc::from_text(&text).expect("parse");
    let mut shell2 = EditorShell::new(None, Some(root.clone()));
    let ctx2 = egui::Context::default();
    ctx2.set_embed_viewports(false);
    regia_editor::editor_layout::LayoutRequests::push(
        world,
        regia_editor::editor_layout::LayoutRequest::Apply(restored.clone()),
    );
    let back = frames(&mut shell2, &ctx2, world, 3);
    assert_eq!(back.listed().len(), restored.floating.len());
    let content = restored
        .floating
        .iter()
        .find(|w| w.tabs.iter().any(|t| t == "content"))
        .expect("content window");
    let (x, y, _, _) = content.rect.expect("rect");
    let builder = back
        .listed()
        .into_iter()
        .map(|(_, b)| b)
        .find(|b| b.title.as_deref().is_some_and(|t| t.starts_with("Content")))
        .expect("content window listed");
    assert_eq!(builder.position, Some(MAIN.min + egui::vec2(x, y)));
    let id = back
        .listed()
        .into_iter()
        .find(|(_, b)| b.title.as_deref().is_some_and(|t| t.starts_with("Content")))
        .map(|(id, _)| id)
        .expect("id");
    let serial = status(world).window_of(Panel::Content).expect("floated").serial;
    assert!(
        shell2.os_window_restored(serial),
        "a restored window re-docks if its monitor is gone"
    );
    assert_eq!(shell2.os_window_serial(id), Some(serial));
    let _ = std::fs::remove_dir_all(&config);
    let _ = std::fs::remove_dir_all(&root);
}

/// One extra window never renders the 3D viewport twice: pane pictures per frame are the same with
/// the Inspector in its own OS window, and with the Viewport itself floated (lavapipe).
#[test]
fn an_os_window_never_renders_the_viewport_twice() {
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let config = unique_dir("panes-config");
    let root = temp_project("panes");
    let mut app = edit_app(&config, &root);
    app.world_mut().insert_resource(OsWindowSupport::default());
    let ctx = egui::Context::default();
    ctx.set_embed_viewports(false);
    // Realtime: the pane renders every frame, so the count is per frame.
    app.world_mut()
        .resource_mut::<regia_editor::viewport::ViewportSettings>()
        .realtime = true;
    let mut shell = EditorShell::new(Some(device), Some(root.clone()));
    let world = app.world_mut();
    let pictures = |world: &World| {
        world
            .get_resource::<regia_editor::viewport::ViewportStats>()
            .map_or(0, |s| s.pane_frames)
    };
    // Warm up, then count pane pictures over the same number of frames in each arrangement.
    const N: u32 = 4;
    frames(&mut shell, &ctx, world, 6);
    let start = pictures(world);
    frames(&mut shell, &ctx, world, N);
    let docked = pictures(world) - start;
    assert!(docked >= u64::from(N), "the viewport draws every frame: {docked}");

    call(world, "editor.panel_float", json!({ "panel": "inspector" })).expect("float");
    frames(&mut shell, &ctx, world, 3);
    let start = pictures(world);
    let last = frames(&mut shell, &ctx, world, N);
    assert_eq!(last.windows.len(), 1);
    assert_eq!(
        pictures(world) - start,
        docked,
        "one more window, no more pane renders"
    );

    call(world, "editor.panel_float", json!({ "panel": "viewport" })).expect("float");
    frames(&mut shell, &ctx, world, 3);
    let start = pictures(world);
    let last = frames(&mut shell, &ctx, world, N);
    assert_eq!(last.windows.len(), 2);
    assert_eq!(
        pictures(world) - start,
        docked,
        "the viewport in its own window renders once per frame, there only"
    );
    let _ = std::fs::remove_dir_all(&config);
    let _ = std::fs::remove_dir_all(&root);
}

/// Graph editors pop out like any panel (P-012): the material graph, the anim graph, the
/// sequence curves, the montage timeline and the procedural graph each get an OS window, draw
/// there, and the layout remembers them.
#[test]
fn graph_editors_pop_out_to_os_windows_and_the_layout_remembers_them() {
    let (mut app, mut shell, ctx, config, root) = os_setup("graphs");
    let world = app.world_mut();
    frames(&mut shell, &ctx, world, 2);
    let graphs = [
        Panel::MaterialGraph,
        Panel::AnimGraph,
        Panel::SeqCurves,
        Panel::Montage,
        Panel::ProcGraph,
    ];
    for panel in graphs {
        call(world, "editor.panel_replace", json!({ "to": panel.id() })).expect("open");
        frames(&mut shell, &ctx, world, 2);
        call(
            world,
            "editor.panel_float",
            json!({ "panel": panel.id(), "os_window": true }),
        )
        .unwrap_or_else(|e| panic!("{}: {e}", panel.id()));
    }
    let out = frames(&mut shell, &ctx, world, 3);
    assert_eq!(out.listed().len(), graphs.len(), "{:?}", out.listed());
    assert_eq!(out.windows.len(), graphs.len());
    for panel in graphs {
        let window = status(world).window_of(panel).cloned().expect("floated");
        assert!(window.os_window, "{}", panel.id());
        let (_, pass) = out
            .windows
            .iter()
            .find(|(id, _)| *id == os_windows::viewport_id(window.serial))
            .expect("its pass ran");
        assert!(
            has_text(&pass.shapes, panel.title()),
            "{} draws in its window",
            panel.id()
        );
        assert!(
            !has_text(&out.root.shapes, panel.title()),
            "{} left the main window",
            panel.id()
        );
    }
    let doc = shell.layout();
    for panel in graphs {
        assert!(
            doc.floating
                .iter()
                .any(|w| w.tabs == [panel.id()] && w.rect.is_some()),
            "{} saved as a window: {:?}",
            panel.id(),
            doc.floating
        );
    }
    let _ = std::fs::remove_dir_all(&config);
    let _ = std::fs::remove_dir_all(&root);
}
