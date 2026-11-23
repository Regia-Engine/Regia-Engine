// SPDX-License-Identifier: MPL-2.0
//! The Diagnostics window and its agent surface (ADR-2771), headless: the GPU tab state, the crash
//! view, the `.regiatrace` timeline through `editor.trace_timeline` and `editor.trace_record`, and
//! the shell restoring floating dock windows. No display and no golden images.
//!
//! The trace recorder and the crash directory are process-wide, so one test owns them.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_editor::editor_layout::{self, FloatingWindow, LayoutDoc};
use regia_editor::shell::EditorShell;
use serde_json::{Value, json};

fn edit_world() -> App {
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
    regia_agent::trace::install_trace_methods(app.world_mut());
    regia_agent::breadcrumbs::install_breadcrumb_read_methods(app.world_mut());
    app
}

/// Calls a registered method; returns `data.code` (or the message) on failure.
fn call(world: &mut World, method: &str, params: Value) -> Result<Value, String> {
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params).map_err(|e| {
        e.data
            .as_ref()
            .and_then(|d| d.get("code").or_else(|| d.get("reason")))
            .and_then(Value::as_str)
            .map_or_else(|| e.message.clone(), str::to_owned)
    })
}

fn names(items: &Value) -> Vec<String> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_owned())
        .collect()
}

const REPORT: &str = "device_lost: unknown: hung in ssao\nlocation: regia_render::gpu_health\n\
    gpu breadcrumbs: level 3 (timestamped), submitted 9, completed 8, acknowledged frame 4\n  \
    suspects (2): ssao, opaque\nbacktrace: none\n";

#[test]
fn the_diagnostics_window_reads_what_the_agent_methods_read() {
    let dir = regia_test_utils::unique_temp_dir("diagnostics");
    let profiling = dir.join("profiling");
    let crashes = dir.join("logs");
    std::fs::create_dir_all(&crashes).unwrap();
    regia::telemetry::trace::configure(regia::telemetry::trace::TraceConfig {
        dir: profiling.clone(),
        ..Default::default()
    });
    regia_platform::crash::configure(
        Some(crashes.clone()),
        regia_platform::crash::BuildInfo::detect(),
        regia_platform::OsInfo::detect(),
    );
    let mut app = edit_world();
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
    let world = app.world_mut();

    // The GPU tab: defaults, then opened on the timeline tab by an agent.
    let state = call(world, "editor.diagnostics", json!({})).unwrap();
    assert_eq!(
        (state["open"].clone(), state["tab"].clone()),
        (json!(false), json!("gpu"))
    );
    assert!(
        state["gpu"]["level"].is_string() || state["gpu"]["unavailable"].is_string(),
        "the numbers, or why there are none: {state}"
    );
    let state = call(
        world,
        "editor.diagnostics",
        json!({ "open": true, "tab": "timeline" }),
    )
    .unwrap();
    assert_eq!(
        (state["open"].clone(), state["tab"].clone()),
        (json!(true), json!("timeline"))
    );
    assert_eq!(
        call(world, "editor.diagnostics", json!({ "tab": "cpu" })).unwrap_err(),
        "bad_tab"
    );

    // Load last crash: nothing yet, then a report with a block.
    let none = call(world, "editor.diagnostics", json!({ "load_crash": true })).unwrap();
    assert!(
        none["crash"].is_null() && none["crash_error"].is_string(),
        "{none}"
    );
    std::fs::write(crashes.join("crash-2026-11-05_10-00-00.txt"), REPORT).unwrap();
    let loaded = call(world, "editor.diagnostics", json!({ "load_crash": true })).unwrap();
    let block = loaded["crash"]["block"].as_str().unwrap();
    assert!(
        block.starts_with("gpu breadcrumbs: level 3 (timestamped)"),
        "{block}"
    );
    assert!(
        block.contains("suspects (2): ssao, opaque") && !block.contains("backtrace"),
        "{block}"
    );
    assert_eq!(loaded["crash"]["headline"], "device_lost: unknown: hung in ssao");

    // Record from the button's twin, mark and range while writing, stop: the file opens.
    assert_eq!(
        call(world, "editor.trace_record", json!({ "action": "pause" })).unwrap_err(),
        "bad_action"
    );
    assert_eq!(
        call(world, "editor.trace_record", json!({ "action": "stop" })).unwrap_err(),
        "trace_not_writing"
    );
    call(
        world,
        "editor.trace_record",
        json!({ "action": "start", "file": "diag" }),
    )
    .unwrap();
    call(world, "telemetry.mark", json!({ "name": "boot" })).unwrap();
    let open = call(world, "telemetry.range_open", json!({ "name": "load" })).unwrap();
    let inner = call(
        world,
        "telemetry.range_open",
        json!({ "name": "parse", "lane": "render" }),
    )
    .unwrap();
    call(world, "telemetry.range_close", json!({ "id": inner["id"] })).unwrap();
    call(world, "telemetry.range_close", json!({ "id": open["id"] })).unwrap();
    call(world, "telemetry.mark", json!({ "name": "done" })).unwrap();
    let stopped = call(world, "editor.trace_record", json!({ "action": "stop" })).unwrap();
    assert_eq!(stopped["opened_in_timeline"], true, "{stopped}");

    let timeline = call(world, "editor.trace_timeline", json!({})).unwrap();
    assert_eq!(timeline["loaded"], true);
    let lanes: Vec<_> = timeline["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["name"].clone())
        .collect();
    assert_eq!(lanes, [json!("main"), json!("render")]);
    let order = names(&timeline["items"]);
    let at = |n: &str| {
        order
            .iter()
            .position(|x| x == n)
            .unwrap_or_else(|| panic!("{n} in {order:?}"))
    };
    assert!(at("boot") < at("load") && at("load") < at("done"), "{order:?}");
    assert!(order.contains(&"parse".to_owned()));
    let item = |n: &str| timeline["items"][at(n)].clone();
    assert_eq!(
        (item("boot")["kind"].clone(), item("load")["kind"].clone()),
        (json!("mark"), json!("range"))
    );
    assert_eq!(item("parse")["lane"], "render");

    // Zoom in on the end: the start is out of view, the layout still answers.
    let end = call(world, "editor.trace_timeline", json!({ "zoom": 1.0e6, "pan": 1 })).unwrap();
    assert!(end["items_total"].as_u64().unwrap() <= timeline["items_total"].as_u64().unwrap());
    assert!(!names(&end["items"]).contains(&"boot".to_owned()), "{end}");
    assert_eq!(
        call(world, "editor.trace_timeline", json!({ "zoom": "big" })).unwrap_err(),
        "bad_view"
    );
    let back = call(
        world,
        "editor.trace_timeline",
        json!({ "zoom": 1, "pan": 0, "layout": false }),
    )
    .unwrap();
    assert!(back.get("items").is_none());

    // Damaged files: a clear code, and the open trace stays.
    let file = PathBuf::from(timeline["file"].as_str().unwrap());
    let bytes = std::fs::read(&file).unwrap();
    std::fs::write(dir.join("empty.regiatrace"), []).unwrap();
    std::fs::write(dir.join("junk.regiatrace"), b"definitely not a trace file").unwrap();
    std::fs::write(dir.join("cut.regiatrace"), &bytes[..bytes.len() - 5]).unwrap();
    for bad in ["empty.regiatrace", "junk.regiatrace", "missing.regiatrace"] {
        let path = dir.join(bad).display().to_string();
        assert_eq!(
            call(world, "editor.trace_timeline", json!({ "path": path })).unwrap_err(),
            "trace_unreadable",
            "{bad}"
        );
    }
    assert_eq!(
        call(world, "editor.trace_timeline", json!({ "path": "" })).unwrap_err(),
        "bad_path"
    );
    let still = call(world, "editor.trace_timeline", json!({ "layout": false })).unwrap();
    assert_eq!(still["loaded"], true, "a failed open keeps the old trace");
    let cut = call(
        world,
        "editor.trace_timeline",
        json!({ "path": "cut.regiatrace" }),
    )
    .unwrap();
    assert_eq!(cut["loaded"], true, "{cut}");
    assert!(cut["truncated"] == true || cut["warning"].is_null(), "{cut}");
    let closed = call(world, "editor.trace_timeline", json!({ "close": true })).unwrap();
    assert_eq!(closed["loaded"], false);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn floating_windows_survive_a_restart_and_a_smaller_screen() {
    let dir = regia_test_utils::unique_temp_dir("diagnostics-layout");
    std::fs::create_dir_all(dir.join("assets/levels")).unwrap();
    std::fs::write(
        dir.join("Regia.toml"),
        "[project]\nname = \"D\"\nid = \"com.example.d\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("assets/levels/main.level.ron"),
        "( format_version: 1, entities: [] )",
    )
    .unwrap();
    let mut app = edit_world();
    app.world_mut()
        .insert_resource(regia_editor::editor_prefs::EditorConfigPaths::at(dir.join("cfg")));
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
    // The main surface holds only two panels; every other panel is known (so none is added back).
    let mut doc = LayoutDoc::from_dock(&egui_dock::DockState::new(vec![
        regia_editor::shell::Panel::Viewport,
        regia_editor::shell::Panel::Outliner,
    ]));
    doc.floating = vec![
        FloatingWindow {
            tabs: vec!["tables".into(), "cameras".into()],
            active: 1,
            rect: Some((900.0, 500.0, 500.0, 400.0)),
        },
        FloatingWindow {
            tabs: vec!["layers".into()],
            active: 0,
            rect: Some((50.0, 60.0, 300.0, 200.0)),
        },
    ];
    let ctx = egui::Context::default();
    let mut shell = EditorShell::new(None, Some(dir.clone()));
    let frame = |shell: &mut EditorShell, world: &mut World, size: egui::Vec2| {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| shell.ui(ui, world, None));
        out.textures_delta.clear();
    };
    let layout_json = serde_json::to_value(&doc).unwrap();
    call(
        app.world_mut(),
        "editor.layout_apply",
        json!({ "layout": layout_json }),
    )
    .unwrap();
    // A small monitor: 640 x 360 cannot hold the first window at x 900.
    for _ in 0..4 {
        frame(&mut shell, app.world_mut(), egui::vec2(640.0, 360.0));
    }
    let shown = shell.layout();
    assert_eq!(shown.floating.len(), 2, "{:?}", shown.floating);
    assert_eq!(shown.floating[0].tabs, ["tables", "cameras"]);
    assert_eq!(shown.floating[0].active, 1);
    for window in &shown.floating {
        let (x, y, w, h) = window.rect.expect("drawn, so it has a rectangle");
        assert!(
            x >= -0.5 && y >= -0.5 && x + w <= 641.0 && y + h <= 361.0,
            "{window:?}"
        );
    }
    // Saved with the project, a new session shows the windows again.
    call(app.world_mut(), "editor.layout_save", json!({})).unwrap();
    let text = std::fs::read_to_string(editor_layout::project_layout_path(&dir)).unwrap();
    assert!(
        text.contains("format_version: 3") && text.contains("floating"),
        "{text}"
    );
    let mut next = EditorShell::new(None, Some(dir.clone()));
    for _ in 0..4 {
        frame(&mut next, app.world_mut(), egui::vec2(1280.0, 800.0));
    }
    let restored = next.layout();
    assert_eq!(restored.floating.len(), 2);
    assert_eq!(restored.floating[1].tabs, ["layers"]);
    // `editor.layout_get` reports them.
    let got = call(app.world_mut(), "editor.layout_get", json!({})).unwrap();
    assert_eq!(got["format_version"], 3);
    assert!(
        got["layout"]["floating"].as_array().is_some_and(|w| w.len() == 2),
        "{got}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
