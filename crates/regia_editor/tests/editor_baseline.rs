// SPDX-License-Identifier: MPL-2.0
//! The editor baseline (ADR-2321) end to end, headless: preferences, keymap and layout files with
//! versioning and migration, the shell restoring and saving layouts per project and per user, the
//! Project Settings model, and every new agent method (`book/agents/editor-preferences.md`).
//!
//! The last test renders the Preferences, Keymap and Project Settings panels through the editor's
//! window capture on the headless adapter (lavapipe in CI) and writes PNGs for the lead's eye check
//! (`REGIA_EDITOR_SHOTS`, default the temp dir).

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_editor::command_palette::{CommandKeymap, KeyChord};
use regia_editor::editor_keymap::{self, ImportMode, KeymapProblem};
use regia_editor::editor_layout::{self, LayoutDoc, LayoutNode, LayoutSource};
use regia_editor::editor_prefs::{self, EditorConfigPaths, EditorPreferences, PrefValue, PrefsError};
use regia_editor::shell::{EditorShell, Panel};
use serde_json::{Value, json};

fn unique_dir(label: &str) -> PathBuf {
    regia_test_utils::unique_temp_dir(&format!("baseline-{label}"))
}

fn temp_project(label: &str, manifest: &str) -> PathBuf {
    let dir = unique_dir(label);
    std::fs::create_dir_all(dir.join("assets/levels")).expect("assets");
    std::fs::write(dir.join("Regia.toml"), manifest).expect("manifest");
    std::fs::write(
        dir.join("assets/levels/main.level.ron"),
        "( format_version: 1, entities: [] )",
    )
    .expect("level");
    dir
}

fn edit_app() -> App {
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
    app.update();
    app
}

fn set_project(app: &mut App, root: &Path) {
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
}

/// Runs `frames` shell frames without a GPU.
fn frames(shell: &mut EditorShell, ctx: &egui::Context, world: &mut World, frames: u32) {
    for frame in 0..frames {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            time: Some(f64::from(frame) / 60.0),
            predicted_dt: 1.0 / 60.0,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| shell.ui(ui, world, None));
        // No renderer takes the font atlas here.
        output.textures_delta.clear();
    }
}

/// Calls an editor agent method by name (installing the editor surface on a bare world) and
/// returns the error code on failure.
fn call(world: &mut World, method: &str, params: Value) -> Result<Value, String> {
    if world.get_resource::<AgentExtension>().is_none() {
        regia_editor::agent_ext::install(world);
    }
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

// ------------------------------------------------------------------------------------------------
// Preferences

#[test]
fn prefs_round_trip_keep_unknown_keys_and_drop_bad_values() {
    let text = r#"(
        format_version: 1,
        values: {
            "appearance.ui_scale": Float(1.5),
            "camera.fly_speed": Int(20),
            "autosave.interval_minutes": Int(999),
            "appearance.theme": Text("neon"),
            "viewport.grid": Text("yes"),
            "from.a.newer.editor": Bool(true),
        },
    )"#;
    let (prefs, diagnostics) = EditorPreferences::from_text(text).expect("reads");
    assert_eq!(prefs.float("appearance.ui_scale"), 1.5);
    assert_eq!(
        prefs.float("camera.fly_speed"),
        20.0,
        "an integer reads as a float"
    );
    assert_eq!(prefs.int("autosave.interval_minutes"), 5, "out of range: default");
    assert_eq!(
        prefs.text("appearance.theme"),
        "default",
        "unknown choice: default"
    );
    assert!(prefs.bool("viewport.grid"), "wrong kind: default");
    assert_eq!(prefs.unknown_keys(), ["from.a.newer.editor"]);
    assert_eq!(diagnostics.len(), 4, "{diagnostics:?}");
    // The unknown key survives a save; a second read gives the same text.
    let saved = prefs.to_text();
    assert!(saved.contains("from.a.newer.editor"), "{saved}");
    let (again, _) = EditorPreferences::from_text(&saved).expect("re-reads");
    assert_eq!(again.to_text(), saved);
}

#[test]
fn prefs_versions_migrate_or_refuse() {
    // No version field: read as version 1.
    let (prefs, _) =
        EditorPreferences::from_text(r#"(values: { "viewport.grid": Bool(false) })"#).expect("unversioned");
    assert!(!prefs.bool("viewport.grid"));
    assert!(prefs.to_text().contains("format_version: 1"));
    assert_eq!(
        EditorPreferences::from_text("(format_version: 9, values: {})").unwrap_err(),
        PrefsError::Newer { found: 9 }
    );
    assert!(matches!(
        EditorPreferences::from_text("(format_version: 0)"),
        Err(PrefsError::Parse(_))
    ));
    assert!(matches!(
        EditorPreferences::from_text("not ron at all"),
        Err(PrefsError::Parse(_))
    ));
    assert!(
        editor_prefs::prefs_migration_sources().is_empty(),
        "version 1 is the first"
    );
}

#[test]
fn prefs_search_set_and_reset() {
    let rows = EditorPreferences::search("scale");
    assert_eq!(rows[0].key, "appearance.ui_scale");
    assert!(
        EditorPreferences::search("crash recovery")
            .iter()
            .any(|d| d.key == "autosave.interval_minutes")
    );
    assert_eq!(EditorPreferences::search("").len(), editor_prefs::PREFS.len());
    assert!(EditorPreferences::search("zzzzqqq").is_empty());

    let mut prefs = EditorPreferences::default();
    let g0 = prefs.generation();
    assert_eq!(prefs.set("camera.fly_speed", &PrefValue::Float(9.0)), Ok(true));
    assert!(prefs.generation() > g0);
    assert_eq!(
        prefs.set("camera.fly_speed", &PrefValue::Float(9.0)),
        Ok(false),
        "no change"
    );
    assert_eq!(
        prefs
            .set("camera.fly_speed", &PrefValue::Float(f64::NAN))
            .unwrap_err()
            .code(),
        "bad_pref_value"
    );
    assert_eq!(
        prefs.set("nope", &PrefValue::Bool(true)).unwrap_err().code(),
        "unknown_pref"
    );
    // Setting the default stores nothing.
    assert_eq!(prefs.set("camera.fly_speed", &PrefValue::Float(6.0)), Ok(true));
    assert!(prefs.is_default("camera.fly_speed"));
    prefs.set("viewport.grid", &PrefValue::Bool(false)).expect("set");
    assert_eq!(prefs.reset("viewport.grid"), Ok(true));
    assert_eq!(prefs.reset("viewport.grid"), Ok(false));
    prefs.set("viewport.grid", &PrefValue::Bool(false)).expect("set");
    prefs
        .set("appearance.ui_scale", &PrefValue::Float(2.0))
        .expect("set");
    assert_eq!(prefs.reset_all(), 2);
    for def in editor_prefs::PREFS {
        assert!(prefs.is_default(def.key), "{}", def.key);
        assert_eq!(prefs.get(def.key), Some(def.default.value()), "{}", def.key);
    }
}

#[test]
fn a_newer_prefs_file_is_never_overwritten() {
    let dir = unique_dir("prefs-newer");
    let file = dir.join(editor_prefs::PREFS_FILE);
    std::fs::write(&file, "(format_version: 42, values: { \"x\": Bool(true) })").expect("write");
    let mut world = World::new();
    world.insert_resource(EditorConfigPaths::at(&dir));
    editor_prefs::load_prefs(&mut world);
    assert!(world.resource::<editor_prefs::PrefsStore>().read_only.is_some());
    world
        .resource_mut::<EditorPreferences>()
        .set("viewport.grid", &PrefValue::Bool(false))
        .expect("set");
    assert!(!editor_prefs::persist_prefs(&mut world), "read-only");
    assert!(
        std::fs::read_to_string(&file)
            .expect("still there")
            .contains("42")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prefs_apply_only_what_changed_and_persist() {
    let dir = unique_dir("prefs-apply");
    let mut world = World::new();
    world.insert_resource(EditorConfigPaths::at(&dir));
    world.insert_resource(regia_editor::viewport::EditorCamera::default());
    editor_prefs::load_prefs(&mut world);
    // Defaults: the first apply touches nothing.
    assert!(editor_prefs::apply_prefs(None, &mut world).is_empty());
    assert!(!editor_prefs::persist_prefs(&mut world), "nothing to write");
    assert!(!dir.join(editor_prefs::PREFS_FILE).exists());

    let out = call(
        &mut world,
        "editor.prefs_set",
        json!({ "values": { "camera.fly_speed": 15, "appearance.theme": "regia-light" } }),
    )
    .expect("set");
    assert_eq!(out["changed"].as_array().map(Vec::len), Some(2));
    let applied = editor_prefs::apply_prefs(None, &mut world);
    assert_eq!(applied, ["appearance.theme", "camera.fly_speed"]);
    assert_eq!(
        world.resource::<regia_editor::viewport::EditorCamera>().speed,
        15.0
    );
    assert_eq!(
        *world.resource::<regia_editor::ui_roles::ThemeSource>(),
        regia_editor::ui_roles::ThemeSource::Ui("regia-light".into())
    );
    assert!(editor_prefs::persist_prefs(&mut world));
    assert!(!editor_prefs::persist_prefs(&mut world), "written once");

    // A new session reads it back and applies the non-defaults on its first apply.
    let mut next = World::new();
    next.insert_resource(EditorConfigPaths::at(&dir));
    editor_prefs::load_prefs(&mut next);
    assert_eq!(
        next.resource::<EditorPreferences>().float("camera.fly_speed"),
        15.0
    );
    assert_eq!(
        editor_prefs::apply_prefs(None, &mut next),
        ["appearance.theme", "camera.fly_speed"]
    );
    // Edits made before a session reads the file win over it, and are saved.
    let mut third = World::new();
    third.insert_resource(EditorConfigPaths::at(&dir));
    call(
        &mut third,
        "editor.prefs_set",
        json!({ "values": { "camera.fly_speed": 30 } }),
    )
    .expect("set");
    editor_prefs::load_prefs(&mut third);
    let prefs = third.resource::<EditorPreferences>();
    assert_eq!(prefs.float("camera.fly_speed"), 30.0);
    assert_eq!(
        prefs.text("appearance.theme"),
        "regia-light",
        "the file's other values stay"
    );
    assert!(editor_prefs::persist_prefs(&mut third));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prefs_agent_methods() {
    let mut world = World::new();
    let all = call(&mut world, "editor.prefs_get", json!({})).expect("get");
    assert_eq!(all["count"], json!(editor_prefs::PREFS.len()));
    assert!(all["path"].is_null(), "no config paths: session only");
    let scale = call(&mut world, "editor.prefs_get", json!({ "query": "ui scale" })).expect("query");
    assert_eq!(scale["prefs"][0]["key"], "appearance.ui_scale");
    assert_eq!(scale["prefs"][0]["kind"], "float");
    assert_eq!(scale["prefs"][0]["min"], json!(0.5));
    assert_eq!(
        call(&mut world, "editor.prefs_get", json!({ "query": 3 })).unwrap_err(),
        "bad_query"
    );

    // All or nothing: one bad value changes none.
    assert_eq!(
        call(
            &mut world,
            "editor.prefs_set",
            json!({ "values": { "viewport.grid": false, "appearance.ui_scale": 99 } })
        )
        .unwrap_err(),
        "bad_pref_value"
    );
    assert!(
        world
            .get_resource::<EditorPreferences>()
            .is_none_or(|p| p.bool("viewport.grid"))
    );
    assert_eq!(
        call(&mut world, "editor.prefs_set", json!({ "values": { "nope": 1 } })).unwrap_err(),
        "unknown_pref"
    );
    assert_eq!(
        call(&mut world, "editor.prefs_set", json!({})).unwrap_err(),
        "bad_pref_value"
    );
    call(
        &mut world,
        "editor.prefs_set",
        json!({ "values": { "viewport.grid": false, "appearance.ui_scale": 1.25 } }),
    )
    .expect("set");
    let reset = call(
        &mut world,
        "editor.prefs_reset",
        json!({ "keys": ["viewport.grid"] }),
    )
    .expect("reset");
    assert_eq!(reset["reset"], json!(["viewport.grid"]));
    assert_eq!(
        call(&mut world, "editor.prefs_reset", json!({ "keys": ["nope"] })).unwrap_err(),
        "unknown_pref"
    );
    let all = call(&mut world, "editor.prefs_reset", json!({})).expect("reset all");
    assert_eq!(all["reset"], json!(["appearance.ui_scale"]));

    let panel = call(
        &mut world,
        "editor.prefs_panel",
        json!({ "tab": "keymap", "query": "fly" }),
    )
    .expect("panel");
    assert_eq!(panel["tab"], "keymap");
    // The viewport-camera navigation preferences (ADR-7755) also name flying: the title matches
    // first, then the ones whose label or description mentions it. "Revision control: Show
    // lockable files read-only" (w12-vcs-locks) is a fuzzy title match too (f…l…y), so it ranks
    // with the titles, ahead of the description matches.
    assert_eq!(
        panel["matches"],
        json!([
            "camera.fly_speed",
            "camera.invert_fly_y",
            "vcs.lockable_read_only",
            "camera.invert_wheel",
            "camera.speed_with_distance"
        ])
    );
    assert_eq!(
        world
            .resource::<regia_editor::editor_layout::LayoutRequests>()
            .pending,
        [regia_editor::editor_layout::LayoutRequest::Open(
            Panel::Preferences
        )]
    );
    assert_eq!(
        call(&mut world, "editor.prefs_panel", json!({ "tab": "x" })).unwrap_err(),
        "bad_tab"
    );
}

// ------------------------------------------------------------------------------------------------
// Keymap

#[test]
fn keymap_file_round_trip_problems_and_versions() {
    let text = r#"(format_version: 1, bindings: {
        "view.toggle_grid": "Ctrl+G",
        "view.toggle_labels": "Hyper+Q",
        "edit.undo": "Ctrl+Shift+P",
        "from.newer": "F9",
    })"#;
    let read = editor_keymap::parse_keymap(text).expect("reads");
    assert_eq!(read.keymap.bindings.len(), 1);
    let kinds: Vec<&str> = read.problems.iter().map(KeymapProblem::kind).collect();
    assert!(
        kinds.contains(&"bad_chord") && kinds.contains(&"reserved") && kinds.contains(&"unknown_command"),
        "{kinds:?}"
    );
    let out = editor_keymap::keymap_to_text(&read.keymap, &read.unknown);
    assert!(
        out.contains("\"from.newer\": \"F9\""),
        "unknown ids are kept: {out}"
    );
    let again = editor_keymap::parse_keymap(&out).expect("re-reads");
    assert_eq!(editor_keymap::keymap_to_text(&again.keymap, &again.unknown), out);
    assert_eq!(
        editor_keymap::parse_keymap("(format_version: 2, bindings: {})")
            .unwrap_err()
            .code(),
        "keymap_newer"
    );
    assert_eq!(
        editor_keymap::parse_keymap("[1, 2]").unwrap_err().code(),
        "keymap_parse"
    );
    assert!(editor_keymap::keymap_migration_sources().is_empty());
}

#[test]
fn keymap_import_merge_replace_and_conflicts() {
    let mut current = CommandKeymap::default();
    current
        .bind("view.toggle_grid", KeyChord::parse("Ctrl+G").expect("chord"))
        .expect("bind");
    current
        .bind("view.toggle_labels", KeyChord::parse("Ctrl+L").expect("chord"))
        .expect("bind");
    let file = r#"(format_version: 1, bindings: { "view.toggle_realtime": "Ctrl+G", "view.toggle_audio_gizmos": "Ctrl+Z" })"#;
    let merged = editor_keymap::import_keymap(&current, file, ImportMode::Merge).expect("merge");
    assert_eq!(
        merged.moved_from,
        ["view.toggle_grid"],
        "Ctrl+G moves to the imported command"
    );
    assert_eq!(merged.keymap.bindings.len(), 3);
    let hidden: Vec<&str> = merged.conflicts.iter().map(|c| c.keys.as_str()).collect();
    // In command id order: Ctrl+Z (audio gizmos) hides Undo's built-in key, Ctrl+G (realtime)
    // hides the outliner's group key.
    assert_eq!(hidden, ["Ctrl+Z", "Ctrl+G"], "{:?}", merged.conflicts);
    let replaced = editor_keymap::import_keymap(&current, file, ImportMode::Replace).expect("replace");
    assert_eq!(replaced.keymap.bindings.len(), 2);
    assert!(!replaced.keymap.bindings.contains_key("view.toggle_labels"));
}

#[test]
fn keymap_persists_across_sessions_and_through_editor_command() {
    let dir = unique_dir("keymap-persist");
    let mut world = World::new();
    world.insert_resource(EditorConfigPaths::at(&dir));
    editor_keymap::load_keymap(&mut world);
    assert!(!editor_keymap::persist_keymap(&mut world), "nothing changed");
    // The palette's own rebinding (editor.command bind) is saved too.
    call(
        &mut world,
        "editor.command",
        json!({ "bind": { "id": "view.toggle_grid", "keys": "Ctrl+Alt+G" } }),
    )
    .expect("bind");
    assert!(editor_keymap::persist_keymap(&mut world));
    let mut next = World::new();
    next.insert_resource(EditorConfigPaths::at(&dir));
    editor_keymap::load_keymap(&mut next);
    let got = next
        .resource::<CommandKeymap>()
        .bindings
        .get("view.toggle_grid")
        .map(KeyChord::label);
    assert_eq!(got.as_deref(), Some("Ctrl+Alt+G"));
    // A binding made before the read wins over the file's and is saved.
    let mut third = World::new();
    third.insert_resource(EditorConfigPaths::at(&dir));
    call(
        &mut third,
        "editor.keymap_bind",
        json!({ "bindings": { "view.toggle_labels": "Ctrl+Alt+G" } }),
    )
    .expect("bind");
    editor_keymap::load_keymap(&mut third);
    let k = third.resource::<CommandKeymap>();
    assert_eq!(
        k.bindings
            .get("view.toggle_labels")
            .map(KeyChord::label)
            .as_deref(),
        Some("Ctrl+Alt+G")
    );
    assert!(!k.bindings.contains_key("view.toggle_grid"), "the chord moved");
    assert!(editor_keymap::persist_keymap(&mut third));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn keymap_agent_methods() {
    let dir = unique_dir("keymap-agent");
    let mut world = World::new();
    let got = call(&mut world, "editor.keymap_get", json!({})).expect("get");
    assert!(
        got["commands"]
            .as_array()
            .is_some_and(|c| c.iter().any(|c| c["id"] == "window.preferences"))
    );
    let bound = call(
        &mut world,
        "editor.keymap_bind",
        json!({ "bindings": { "view.toggle_grid": "Ctrl+Z", "window.preferences": "Ctrl+Comma" } }),
    )
    .expect("bind");
    assert_eq!(bound["conflicts"][0]["command"], "view.toggle_grid");
    assert_eq!(
        call(
            &mut world,
            "editor.keymap_bind",
            json!({ "bindings": { "x.y": "F1" } })
        )
        .unwrap_err(),
        "unknown_command"
    );
    assert_eq!(
        call(
            &mut world,
            "editor.keymap_bind",
            json!({ "bindings": { "view.toggle_grid": "Nope+1" } })
        )
        .unwrap_err(),
        "bad_binding"
    );
    assert_eq!(
        call(
            &mut world,
            "editor.keymap_bind",
            json!({ "bindings": { "view.toggle_grid": "Ctrl+Shift+P" } })
        )
        .unwrap_err(),
        "bad_binding"
    );
    // An export writes only a `*keymap.ron` file (rc2 review, paths finding 1).
    let file = dir.join("exported.keymap.ron");
    let exported = call(
        &mut world,
        "editor.keymap_export",
        json!({ "path": file.display().to_string() }),
    )
    .expect("export");
    assert_eq!(exported["bindings"], 2);
    let cleared = call(&mut world, "editor.keymap_reset", json!({})).expect("reset");
    assert_eq!(cleared["cleared"], 2);
    let dry = call(
        &mut world,
        "editor.keymap_import",
        json!({ "path": file.display().to_string(), "dry_run": true }),
    )
    .expect("dry run");
    assert_eq!(dry["import"]["applied"].as_array().map(Vec::len), Some(2));
    assert!(
        world.resource::<CommandKeymap>().bindings.is_empty(),
        "a dry run changes nothing"
    );
    call(
        &mut world,
        "editor.keymap_import",
        json!({ "path": file.display().to_string(), "mode": "replace" }),
    )
    .expect("import");
    assert_eq!(world.resource::<CommandKeymap>().bindings.len(), 2);
    assert_eq!(
        call(
            &mut world,
            "editor.keymap_import",
            json!({ "text": "(format_version: 1)", "mode": "sideways" })
        )
        .unwrap_err(),
        "bad_mode"
    );
    assert_eq!(
        call(&mut world, "editor.keymap_import", json!({ "text": "{{" })).unwrap_err(),
        "keymap_parse"
    );
    assert_eq!(
        call(&mut world, "editor.keymap_export", json!({ "path": "rel.ron" })).unwrap_err(),
        "bad_path"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------------------------------------
// Layout

#[test]
fn layout_normalizes_unknown_repeated_and_new_panels() {
    let doc = LayoutDoc {
        format_version: 1,
        root: LayoutNode::Split {
            axis: editor_layout::SplitAxis::Horizontal,
            fraction: f32::NAN,
            first: Box::new(LayoutNode::Tabs {
                tabs: vec!["viewport".into(), "gone_panel".into(), "viewport".into()],
                active: 7,
            }),
            second: Box::new(LayoutNode::Split {
                axis: editor_layout::SplitAxis::Vertical,
                fraction: 3.0,
                first: Box::new(LayoutNode::Tabs {
                    tabs: vec!["ext".into()],
                    active: 0,
                }),
                second: Box::new(LayoutNode::Tabs {
                    tabs: vec!["content".into(), "outliner".into()],
                    active: 1,
                }),
            }),
        },
        // Saved by an editor that knew only these: everything else of the default layout is new.
        known_panels: vec![
            "viewport".into(),
            "content".into(),
            "outliner".into(),
            "inspector".into(),
        ],
        floating: Vec::new(),
        instances: Default::default(),
    };
    let (dock, report) = doc.to_dock().expect("builds");
    assert_eq!(report.dropped, ["gone_panel", "viewport", "ext"]);
    assert!(
        report.added.contains(&"output_log") && report.added.contains(&"cameras"),
        "{report:?}"
    );
    assert!(
        !report.added.contains(&"inspector"),
        "the user closed it; it stays closed"
    );
    assert!(
        !report.added.contains(&"preferences"),
        "not in the default layout"
    );
    let back = LayoutDoc::from_dock(&dock);
    let LayoutNode::Split { fraction, second, .. } = &back.root else {
        panic!("a split: {back:?}");
    };
    assert_eq!(*fraction, 0.5, "NaN becomes an even split");
    let LayoutNode::Tabs { tabs, active } = second.as_ref() else {
        panic!("the empty extension group collapsed its split: {second:?}");
    };
    assert_eq!(&tabs[..2], ["content", "outliner"]);
    assert!(
        tabs.contains(&"output_log".to_owned()),
        "new panels join the content group"
    );
    assert_eq!(*active, 1, "the shown tab survives");
}

#[test]
fn layout_files_refuse_newer_too_deep_and_empty() {
    assert_eq!(
        // One past the current version (3 since ADR-8105 (a)).
        LayoutDoc::from_text(&format!(
            "(format_version: {}, root: Tabs(tabs: [\"viewport\"]))",
            editor_layout::EDITOR_LAYOUT_FORMAT_VERSION + 1
        ))
        .unwrap_err()
        .code(),
        "layout_newer"
    );
    let mut node = LayoutNode::Tabs {
        tabs: vec!["viewport".into()],
        active: 0,
    };
    for _ in 0..=editor_layout::MAX_DEPTH {
        node = LayoutNode::Split {
            axis: editor_layout::SplitAxis::Vertical,
            fraction: 0.5,
            first: Box::new(node),
            second: Box::new(LayoutNode::Tabs {
                tabs: vec!["content".into()],
                active: 0,
            }),
        };
    }
    let deep = LayoutDoc {
        format_version: 1,
        root: node,
        known_panels: Vec::new(),
        floating: Vec::new(),
        instances: Default::default(),
    };
    assert_eq!(
        LayoutDoc::from_text(&deep.to_text()).unwrap_err().code(),
        "layout_invalid"
    );
    let empty = LayoutDoc {
        format_version: 1,
        root: LayoutNode::Tabs {
            tabs: vec!["nothing".into()],
            active: 0,
        },
        known_panels: Vec::new(),
        floating: Vec::new(),
        instances: Default::default(),
    };
    assert_eq!(empty.to_dock().unwrap_err().code(), "layout_invalid");
    // A newer file in the way is not overwritten.
    let dir = unique_dir("layout-newer");
    let file = dir.join("editor_layout.ron");
    std::fs::write(&file, "(format_version: 5, root: Tabs(tabs: []))").expect("write");
    let doc = LayoutDoc::from_dock(&editor_layout::default_dock());
    assert_eq!(
        editor_layout::write_layout_file(&file, &doc).unwrap_err().code(),
        "layout_newer"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn shell_restores_saves_resets_and_reopens_layouts() {
    let config = unique_dir("layout-user");
    let root = temp_project(
        "layout-project",
        "[project]\nname = \"L\"\nid = \"com.example.l\"\n",
    );
    let mut app = edit_app();
    app.world_mut().insert_resource(EditorConfigPaths::at(&config));
    set_project(&mut app, &root);
    let ctx = egui::Context::default();
    let mut shell = EditorShell::new(None, Some(root.clone()));
    frames(&mut shell, &ctx, app.world_mut(), 2);
    let default_doc = LayoutDoc::from_dock(&editor_layout::default_dock());
    assert_eq!(shell.layout(), default_doc, "no saved layout: the built-in one");
    let got = call(app.world_mut(), "editor.layout_get", json!({})).expect("get");
    assert_eq!(got["source"], "default");
    assert!(
        got["panels"]
            .as_array()
            .is_some_and(|p| p.contains(&json!("viewport")))
    );

    // An agent opens Preferences and saves the layout into the project.
    call(
        app.world_mut(),
        "editor.layout_apply",
        json!({ "open": "preferences" }),
    )
    .expect("open");
    assert_eq!(
        call(app.world_mut(), "editor.layout_apply", json!({ "open": "nope" })).unwrap_err(),
        "unknown_panel"
    );
    assert_eq!(
        call(app.world_mut(), "editor.layout_apply", json!({})).unwrap_err(),
        "bad_layout_request"
    );
    frames(&mut shell, &ctx, app.world_mut(), 1);
    assert!(shell.layout().panel_ids().contains(&"preferences"));
    let saved = call(app.world_mut(), "editor.layout_save", json!({})).expect("save");
    assert!(
        saved["panels"]
            .as_array()
            .is_some_and(|p| p.contains(&json!("preferences")))
    );
    assert!(editor_layout::project_layout_path(&root).exists());
    assert_eq!(
        call(
            app.world_mut(),
            "editor.layout_save",
            json!({ "scope": "elsewhere" })
        )
        .unwrap_err(),
        "bad_scope"
    );

    // Reset goes back to the built-in layout without touching the file.
    call(app.world_mut(), "editor.layout_reset", json!({})).expect("reset");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    assert_eq!(shell.layout(), default_doc);
    assert!(editor_layout::project_layout_path(&root).exists());

    // A new session opens with the project's layout.
    let mut next = EditorShell::new(None, Some(root.clone()));
    frames(&mut next, &ctx, app.world_mut(), 1);
    assert_eq!(next.layout_source(), LayoutSource::Project);
    assert!(next.layout().panel_ids().contains(&"preferences"));

    // Apply a custom layout from JSON, save it as the user's default; another project uses it.
    let custom = json!({
        "format_version": 1,
        "root": { "Split": { "axis": "Horizontal", "fraction": 0.3,
            "first": { "Tabs": { "tabs": ["content"], "active": 0 } },
            "second": { "Tabs": { "tabs": ["viewport", "inspector"], "active": 0 } } } },
        "known_panels": editor_layout::BUILT_IN_PANELS.iter().map(|p| p.id()).collect::<Vec<_>>(),
    });
    call(
        app.world_mut(),
        "editor.layout_apply",
        json!({ "layout": custom }),
    )
    .expect("apply");
    frames(&mut next, &ctx, app.world_mut(), 1);
    assert_eq!(next.layout_source(), LayoutSource::Session);
    assert_eq!(next.layout().panel_ids(), ["content", "viewport", "inspector"]);
    call(app.world_mut(), "editor.layout_save", json!({ "scope": "user" })).expect("save user");
    assert_eq!(
        call(
            app.world_mut(),
            "editor.layout_apply",
            json!({ "layout": { "root": 3 } })
        )
        .unwrap_err(),
        "layout_parse"
    );

    let other = temp_project(
        "layout-other",
        "[project]\nname = \"O\"\nid = \"com.example.o\"\n",
    );
    set_project(&mut app, &other);
    let mut third = EditorShell::new(None, Some(other.clone()));
    frames(&mut third, &ctx, app.world_mut(), 1);
    assert_eq!(third.layout_source(), LayoutSource::User);
    assert_eq!(third.layout().panel_ids(), ["content", "viewport", "inspector"]);

    // Exit saves into the project unless the preference is off.
    assert_eq!(
        third.save_layout_on_exit(app.world()),
        Some(editor_layout::project_layout_path(&other))
    );
    call(
        app.world_mut(),
        "editor.prefs_set",
        json!({ "values": { "layout.save_on_exit": false, "layout.restore": false } }),
    )
    .expect("prefs");
    assert_eq!(third.save_layout_on_exit(app.world()), None);
    let mut fourth = EditorShell::new(None, Some(other.clone()));
    frames(&mut fourth, &ctx, app.world_mut(), 1);
    assert_eq!(fourth.layout_source(), LayoutSource::Default, "restore is off");
    for dir in [config, root, other] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn palette_commands_open_panels_and_reset_the_layout() {
    let root = temp_project(
        "layout-commands",
        "[project]\nname = \"C\"\nid = \"com.example.c\"\n",
    );
    let mut app = edit_app();
    set_project(&mut app, &root);
    let ctx = egui::Context::default();
    let mut shell = EditorShell::new(None, Some(root.clone()));
    frames(&mut shell, &ctx, app.world_mut(), 1);
    call(
        app.world_mut(),
        "editor.command",
        json!({ "run": "window.preferences" }),
    )
    .expect("run");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    assert!(shell.layout().panel_ids().contains(&Panel::Preferences.id()));
    call(app.world_mut(), "editor.command", json!({ "run": "layout.save" })).expect("run");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    assert!(editor_layout::project_layout_path(&root).exists());
    call(
        app.world_mut(),
        "editor.command",
        json!({ "run": "layout.reset" }),
    )
    .expect("run");
    frames(&mut shell, &ctx, app.world_mut(), 1);
    assert!(!shell.layout().panel_ids().contains(&"preferences"));
    let _ = std::fs::remove_dir_all(root);
}

// ------------------------------------------------------------------------------------------------
// Project Settings

const MANIFEST: &str = r#"# kept comment-free: toml round trips values, not comments
[project]
name = "Settings Yard"
id = "com.example.yard"
version = "1.2.0"
build_number = 7

[platforms.android]
min_sdk = 29
keystore = "env:KEY"

[steam]
app_id = 480

[mystery]
answer = 42
"#;

#[test]
fn project_settings_get_and_set_keep_unknown_keys() {
    let root = temp_project("settings", MANIFEST);
    std::fs::create_dir_all(root.join("config")).expect("config");
    std::fs::write(
        root.join("config/input.ron"),
        r#"(actions: { "Jump": Button, "Fire": Button }, contexts: { "Gameplay": (priority: 0, consume_lower: false, bindings: [(action: "Jump", input: Key("Space")), (action: "Dash", input: Key("ShiftLeft"))]) })"#,
    )
    .expect("input");
    let mut app = edit_app();
    set_project(&mut app, &root);
    let got = call(app.world_mut(), "editor.project_settings_get", json!({})).expect("get");
    assert_eq!(got["project"]["name"], "Settings Yard");
    assert_eq!(got["platforms"]["android"]["enabled"], true);
    assert_eq!(got["platforms"]["linux"]["enabled"], false);
    assert_eq!(got["levels"], json!(["levels/main.level.ron"]));
    assert!(
        got["render_profiles"]
            .as_array()
            .is_some_and(|p| p.contains(&json!("MobileHigh")))
    );
    let input = &got["input"];
    assert!(input["error"].is_null(), "input parses: {input}");
    assert_eq!(input["actions"].as_array().map(Vec::len), Some(2));
    let problems = input["problems"].as_array().expect("problems");
    assert!(
        problems
            .iter()
            .any(|p| p.as_str().is_some_and(|p| p.contains("Dash"))),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.as_str().is_some_and(|p| p.contains("Fire"))),
        "{problems:?}"
    );

    let set = call(
        app.world_mut(),
        "editor.project_settings_set",
        json!({
            "project": { "version": "1.3.0", "startup_level": "levels/main.level.ron", "organization": "Example" },
            "platforms": { "android": { "render_profile": "mobile_high" }, "linux": { "enabled": true, "render_profile": "DesktopHigh" } },
        }),
    )
    .expect("set");
    assert_eq!(set["saved"], true);
    assert_eq!(
        set["platforms"]["android"]["render_profile"], "MobileHigh",
        "canonical name"
    );
    let text = std::fs::read_to_string(root.join("Regia.toml")).expect("manifest");
    for kept in [
        "answer = 42",
        "app_id = 480",
        "keystore = \"env:KEY\"",
        "min_sdk = 29",
    ] {
        assert!(text.contains(kept), "{kept} lost:\n{text}");
    }
    assert!(text.contains("[platforms.linux]"), "{text}");
    assert!(
        app.world()
            .get_resource::<regia_editor::project_settings_ext::ProjectSettingsSaved>()
            .is_some()
    );

    // Refused values write nothing.
    let before = std::fs::read_to_string(root.join("Regia.toml")).expect("manifest");
    for (params, code) in [
        (
            json!({ "platforms": { "android": { "render_profile": "Potato" } } }),
            "bad_render_profile",
        ),
        (
            json!({ "platforms": { "amiga": { "enabled": true } } }),
            "unknown_platform",
        ),
        (
            json!({ "project": { "startup_level": "../escape.level.ron" } }),
            "project_settings_invalid",
        ),
        (
            json!({ "project": { "version": "" } }),
            "project_settings_invalid",
        ),
        (json!({ "project": { "build_number": "seven" } }), "bad_settings"),
        (json!({ "project": { "mystery": "x" } }), "bad_settings"),
    ] {
        assert_eq!(
            call(app.world_mut(), "editor.project_settings_set", params.clone()).unwrap_err(),
            code,
            "{params}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.join("Regia.toml")).expect("manifest"),
        before
    );

    // Removing a target drops its table; save false only validates.
    let dry = call(
        app.world_mut(),
        "editor.project_settings_set",
        json!({ "platforms": { "android": { "enabled": false } }, "save": false }),
    )
    .expect("dry");
    assert_eq!(dry["platforms"]["android"]["enabled"], false);
    assert_eq!(
        std::fs::read_to_string(root.join("Regia.toml")).expect("manifest"),
        before
    );

    let mut world = World::new();
    assert_eq!(
        call(&mut world, "editor.project_settings_get", json!({})).unwrap_err(),
        "no_project"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn project_input_maps_edit_through_the_agent() {
    let root = temp_project("input-edit", MANIFEST);
    let mut app = edit_app();
    set_project(&mut app, &root);
    let file = root.join("config/input.ron");
    // No file yet: the first edit creates it.
    let out = call(
        app.world_mut(),
        "editor.project_input_edit",
        json!({ "edits": [
            { "op": "add_action", "name": "Jump", "kind": "Button" },
            { "op": "add_binding", "action": "Jump", "input": "Key(\"Space\")" },
            { "op": "add_action", "name": "Move", "kind": "Axis2D" },
            { "op": "add_binding", "context": "Menu", "action": "Jump", "input": "GamepadButton(South)" },
        ] }),
    )
    .expect("edit");
    let input = &out["input"];
    assert_eq!(input["actions"].as_array().map(Vec::len), Some(2));
    assert_eq!(input["contexts"].as_array().map(Vec::len), Some(2), "{input}");
    let text = std::fs::read_to_string(&file).expect("written");
    let config = regia_input::InputConfig::from_ron(&text).expect("the runtime reads it");
    assert_eq!(config.gameplay().len(), 1);

    // Refused edits write nothing, and a batch is all or nothing.
    for (edits, code) in [
        (
            json!([{ "op": "add_action", "name": "Jump", "kind": "Button" }]),
            "bad_input_edit",
        ),
        (
            json!([{ "op": "add_action", "name": " ", "kind": "Button" }]),
            "bad_input_edit",
        ),
        (
            json!([{ "op": "add_action", "name": "X", "kind": "Wobble" }]),
            "bad_input_edit",
        ),
        (
            json!([{ "op": "add_binding", "action": "Nope", "input": "Key(\"A\")" }]),
            "bad_input_edit",
        ),
        (
            json!([{ "op": "add_binding", "action": "Move", "input": "Teleport" }]),
            "bad_input_source",
        ),
        (
            json!([{ "op": "remove_binding", "context": "Gameplay", "index": 9 }]),
            "bad_input_edit",
        ),
        (
            json!([{ "op": "remove_binding", "context": "Nowhere", "index": 0 }]),
            "bad_input_edit",
        ),
        (json!([{ "op": "explode" }]), "bad_input_edit"),
        (
            json!([{ "op": "remove_action", "name": "Move" }, { "op": "remove_action", "name": "Nope" }]),
            "bad_input_edit",
        ),
    ] {
        assert_eq!(
            call(
                app.world_mut(),
                "editor.project_input_edit",
                json!({ "edits": edits.clone() })
            )
            .unwrap_err(),
            code,
            "{edits}"
        );
        assert_eq!(
            std::fs::read_to_string(&file).expect("file"),
            text,
            "{edits} wrote"
        );
    }
    assert_eq!(
        call(app.world_mut(), "editor.project_input_edit", json!({})).unwrap_err(),
        "bad_input_edit"
    );

    // Removing an action removes its bindings in every context.
    let out = call(
        app.world_mut(),
        "editor.project_input_edit",
        json!({ "edits": [{ "op": "remove_action", "name": "Jump" }] }),
    )
    .expect("remove");
    assert_eq!(
        out["input"]["actions"],
        json!([{ "name": "Move", "kind": "Axis2D" }])
    );
    for context in out["input"]["contexts"].as_array().expect("contexts") {
        assert_eq!(context["bindings"], json!([]), "{context}");
    }
    // Project settings show the same input maps.
    let got = call(app.world_mut(), "editor.project_settings_get", json!({})).expect("get");
    assert_eq!(got["input"]["actions"], out["input"]["actions"]);
    let _ = std::fs::remove_dir_all(root);
}

// ------------------------------------------------------------------------------------------------
// Screenshots for the eye check (lavapipe in CI).

#[test]
fn baseline_panels_screenshots() {
    let device = match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let root = temp_project("shots", MANIFEST);
    let config = unique_dir("shots-config");
    let mut app = edit_app();
    app.world_mut().insert_resource(EditorConfigPaths::at(&config));
    set_project(&mut app, &root);
    call(
        app.world_mut(),
        "editor.keymap_bind",
        json!({ "bindings": { "view.toggle_grid": "Ctrl+Z", "window.preferences": "Ctrl+Comma" } }),
    )
    .expect("bind");
    call(
        app.world_mut(),
        "editor.prefs_set",
        json!({ "values": { "camera.fly_speed": 12.5 } }),
    )
    .expect("prefs");
    let shots = regia_core::brand::env_var("REGIA_EDITOR_SHOTS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("regia-editor-shots"));
    std::fs::create_dir_all(&shots).expect("shot dir");
    let mut shell = EditorShell::new(Some(device.clone()), Some(root.clone()));
    let shoot = |shell: &mut EditorShell, app: &mut App, name: &str| {
        // Each capture builds its own renderer, so it needs a context that has sent nothing yet.
        let ctx = egui::Context::default();
        let image = regia_editor::window_capture::capture_editor_window(
            shell,
            &ctx,
            app.world_mut(),
            &device,
            [1280, 800],
            1.0,
            3,
        )
        .expect("capture");
        let bytes: Vec<u8> = image
            .pixels
            .iter()
            .flat_map(|p| [p.r(), p.g(), p.b(), p.a()])
            .collect();
        let png = image::RgbaImage::from_raw(1280, 800, bytes).expect("size");
        let path = shots.join(name);
        png.save(&path).expect("png");
        eprintln!("baseline shot: {}", path.display());
        // Not a blank frame: the panel drew text and widgets.
        let distinct: std::collections::HashSet<[u8; 4]> = image
            .pixels
            .iter()
            .map(|p| [p.r(), p.g(), p.b(), p.a()])
            .collect();
        assert!(distinct.len() > 16, "{name} looks blank");
    };
    call(
        app.world_mut(),
        "editor.layout_apply",
        json!({ "open": "preferences" }),
    )
    .expect("open");
    shoot(&mut shell, &mut app, "preferences.png");
    call(app.world_mut(), "editor.prefs_panel", json!({ "tab": "keymap" })).expect("keymap tab");
    shoot(&mut shell, &mut app, "keymap.png");
    call(
        app.world_mut(),
        "editor.layout_apply",
        json!({ "open": "project_settings" }),
    )
    .expect("open");
    shoot(&mut shell, &mut app, "project_settings.png");
    for dir in [root, config] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

// ------------------------------------------------------------------------------------------------
// Keymap scopes, lookup and presets; preferences import, export and category reset; contextual
// help (ADR-7767), all through the agent dispatcher.

#[test]
fn scoped_keymap_bind_lookup_and_safety_keys_over_the_agent() {
    let mut world = World::new();
    let bound = call(
        &mut world,
        "editor.keymap_bind",
        json!({ "bindings": { "view.toggle_grid": "X" }, "scope": "viewport" }),
    )
    .expect("viewport X");
    assert!(bound["conflicts"].as_array().is_some_and(Vec::is_empty));
    let bound = call(
        &mut world,
        "editor.keymap_bind",
        json!({ "bindings": { "view.toggle_labels": "X" }, "scope": "timeline" }),
    )
    .expect("timeline X");
    assert!(
        bound["moved"].as_array().is_some_and(Vec::is_empty),
        "another scope keeps its X: {bound}"
    );
    let row = |out: &Value, id: &str| {
        out["commands"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["id"] == id).cloned())
            .unwrap_or(Value::Null)
    };
    assert_eq!(row(&bound, "view.toggle_grid")["scope"], "viewport");
    assert_eq!(row(&bound, "view.toggle_labels")["scope"], "timeline");
    // No scope: global, as before scopes.
    let bound = call(
        &mut world,
        "editor.keymap_bind",
        json!({ "bindings": { "view.toggle_realtime": "Ctrl+Alt+R" } }),
    )
    .expect("global");
    assert_eq!(row(&bound, "view.toggle_realtime")["scope"], "global");
    // Safety keys stay global.
    assert_eq!(
        call(
            &mut world,
            "editor.keymap_bind",
            json!({ "bindings": { "view.toggle_grid": "Ctrl+S" }, "scope": "viewport" })
        )
        .unwrap_err(),
        "shortcut_safety_key"
    );
    assert_eq!(
        call(
            &mut world,
            "editor.keymap_bind",
            json!({ "bindings": { "view.toggle_grid": "X" }, "scope": "kitchen" })
        )
        .unwrap_err(),
        "bad_scope"
    );
    let found = call(&mut world, "editor.keymap_lookup", json!({ "chord": "X" })).expect("lookup");
    let scopes: Vec<&str> = found["commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["scope"].as_str())
        .collect();
    assert_eq!(scopes, ["viewport", "timeline"], "{found}");
    assert_eq!(found["conflict"], false);
    assert_eq!(
        call(&mut world, "editor.keymap_lookup", json!({ "chord": "Nope+1" })).unwrap_err(),
        "bad_binding"
    );
    // The palette's dispatcher picks the focused panel's binding.
    let keymap = world.resource::<CommandKeymap>().clone();
    let x = KeyChord::parse("X").unwrap();
    assert_eq!(
        editor_keymap::resolve(&keymap, editor_keymap::ShortcutScope::Viewport, x),
        Some("view.toggle_grid")
    );
    assert_eq!(
        editor_keymap::resolve(&keymap, editor_keymap::ShortcutScope::Timeline, x),
        Some("view.toggle_labels")
    );
}

#[test]
fn keymap_presets_over_the_agent() {
    let dir = unique_dir("keymap-presets-agent");
    let mut world = World::new();
    world.insert_resource(EditorConfigPaths::at(&dir));
    let list = call(&mut world, "editor.keymap_preset_list", json!({})).expect("list");
    assert_eq!(list["presets"][0]["name"], "Default");
    assert_eq!(list["presets"][1]["name"], "Laptop");
    let applied = call(
        &mut world,
        "editor.keymap_preset_apply",
        json!({ "name": "Laptop" }),
    )
    .expect("apply");
    assert_eq!(applied["active"], "Laptop");
    assert_eq!(world.resource::<CommandKeymap>().bindings.len(), 2);
    let saved = call(
        &mut world,
        "editor.keymap_preset_save",
        json!({ "name": "travel" }),
    )
    .expect("save");
    assert!(dir.join("keymaps/travel.editor_keymap.ron").is_file(), "{saved}");
    assert_eq!(saved["presets"][2]["name"], "travel");
    assert_eq!(
        call(
            &mut world,
            "editor.keymap_preset_save",
            json!({ "name": "Default" })
        )
        .unwrap_err(),
        "preset_builtin"
    );
    assert_eq!(
        call(
            &mut world,
            "editor.keymap_preset_apply",
            json!({ "name": "nope" })
        )
        .unwrap_err(),
        "preset_unknown"
    );
    assert_eq!(
        call(&mut world, "editor.keymap_preset_save", json!({})).unwrap_err(),
        "preset_name"
    );
    // The preset file is the version 1 keymap shape: import reads it.
    let text = std::fs::read_to_string(dir.join("keymaps/travel.editor_keymap.ron")).unwrap();
    let read = editor_keymap::parse_keymap(&text).unwrap();
    assert!(read.problems.is_empty(), "{:?}", read.problems);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prefs_export_import_and_category_reset_over_the_agent() {
    let dir = unique_dir("prefs-export");
    std::fs::create_dir_all(&dir).unwrap();
    let mut world = World::new();
    call(
        &mut world,
        "editor.prefs_set",
        json!({ "values": { "appearance.ui_scale": 1.5, "camera.fly_speed": 20.0 } }),
    )
    .expect("set");
    let file = dir.join("mine.prefs.ron");
    let exported = call(
        &mut world,
        "editor.prefs_export",
        json!({ "path": file.display().to_string() }),
    )
    .expect("export");
    assert_eq!(exported["values"], 2);
    assert_eq!(
        call(
            &mut world,
            "editor.prefs_export",
            json!({ "path": dir.join("level.ron").display().to_string() })
        )
        .unwrap_err(),
        "bad_path"
    );
    let reset = call(
        &mut world,
        "editor.prefs_reset",
        json!({ "category": "Appearance" }),
    )
    .expect("reset");
    assert_eq!(reset["reset"], json!(["appearance.ui_scale"]));
    assert!(
        !world
            .resource::<EditorPreferences>()
            .is_default("camera.fly_speed")
    );
    assert_eq!(
        call(&mut world, "editor.prefs_reset", json!({ "category": "Nope" })).unwrap_err(),
        "unknown_category"
    );
    let dry = call(
        &mut world,
        "editor.prefs_import",
        json!({ "path": file.display().to_string(), "dry_run": true }),
    )
    .expect("dry");
    assert_eq!(dry["import"]["changed"], json!(["appearance.ui_scale"]));
    assert!(
        world
            .resource::<EditorPreferences>()
            .is_default("appearance.ui_scale")
    );
    call(
        &mut world,
        "editor.prefs_import",
        json!({ "path": file.display().to_string() }),
    )
    .expect("import");
    assert_eq!(
        world.resource::<EditorPreferences>().float("appearance.ui_scale"),
        1.5
    );
    // An agent import never sets the program the editor runs.
    let evil = "(format_version: 1, values: { \"code.editor_command\": Text(\"x {file}\") })";
    let out = call(
        &mut world,
        "editor.prefs_import",
        json!({ "text": evil, "mode": "replace" }),
    )
    .expect("owner");
    assert_eq!(
        out["import"]["skipped_owner_only"],
        json!(["code.editor_command"])
    );
    assert!(
        world
            .resource::<EditorPreferences>()
            .is_default("code.editor_command")
    );
    assert_eq!(
        call(&mut world, "editor.prefs_import", json!({ "text": "{{" })).unwrap_err(),
        "prefs_parse"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn help_context_and_settings_search_over_the_agent() {
    let root = temp_project(
        "help-search",
        "[project]\nname = 'Demo'\n[platforms.android]\nrender_profile = 'mobile'\n",
    );
    let mut world = World::new();
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    let help = call(&mut world, "editor.help_context", json!({ "panel": "content" })).expect("help");
    assert_eq!(help["page"], "user-guide/editor-guide/content-browser.md");
    assert_eq!(help["agent_page"], "agents/content-browser.md");
    assert_eq!(
        call(&mut world, "editor.help_context", json!({ "panel": "nope" })).unwrap_err(),
        "unknown_panel"
    );
    let panel = call(
        &mut world,
        "editor.prefs_panel",
        json!({ "tab": "project", "query": "render_profile" }),
    )
    .expect("panel");
    assert_eq!(panel["tab"], "project");
    assert_eq!(
        panel["project_matches"],
        json!(["platforms.android.render_profile"])
    );
    let _ = std::fs::remove_dir_all(&root);
}
