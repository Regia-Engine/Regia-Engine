// SPDX-License-Identifier: MPL-2.0
//! Fuzz and property tests of the editor's own files (ADR-3290): `editor_prefs.ron`,
//! `editor_keymap.ron`, `editor_layout.ron` (user and project) and the project's `Regia.toml` as
//! Project Settings reads it.
//!
//! - Many seeds of the agent's mutation engine (`regia_agent::fuzz_targets::run`, the one
//!   `fuzz.run` uses) over rich fixtures: no panic, no empty error, and every accepted input
//!   re-encodes to bytes that read back and re-encode to the same bytes (write, read, write gives
//!   identical bytes; stricter than `fuzz.run`'s own check, which accepts an equal RON value).
//! - Every truncation of every fixture and a list of hostile inputs: the readers return a value or
//!   a non-empty error, never panic.
//! - The same hostile bytes as files on disk: the editor loads its defaults and records the error
//!   (`PrefsStore`/`KeymapStore` diagnostics, an `Err` from the layout and settings readers, the
//!   agent's error code), and a shell frame over them draws.
//!
//! These complement `baseline_fuzz`'s single-seed unit test; `regia_editor` is not in xtask's
//! graph, so `cargo xtask fuzz-guided` does not reach these files (ADR-2616).

use std::path::{Path, PathBuf};

use regia_agent::fuzz_targets::{BUG_PREFIX, FuzzInput, FuzzTarget};
use regia_app::prelude::*;
use regia_editor::editor_keymap::{self, KeymapStore};
use regia_editor::editor_layout::{self, LayoutDoc};
use regia_editor::editor_prefs::{self, EditorConfigPaths, EditorPreferences, PrefsStore};
use regia_editor::project_settings::ProjectSettingsState;
use regia_editor::shell::EditorShell;

/// Seeds per target; each runs `ITERATIONS` mutations.
const SEEDS: u64 = 12;
const ITERATIONS: u32 = 600;

// ------------------------------------------------------------------------------------------------
// Fixtures
// ------------------------------------------------------------------------------------------------

/// Every known preference set to a non-default value, plus unknown keys of every value kind.
fn rich_prefs() -> String {
    r#"(
    format_version: 1,
    values: {
        "appearance.theme": Text("regia-light"),
        "appearance.ui_scale": Float(1.5),
        "autosave.interval_minutes": Int(3),
        "camera.fly_speed": Float(40.0),
        "layout.save_on_exit": Bool(false),
        "layout.restore": Bool(false),
        "viewport.grid": Bool(false),
        "code.editor_command": Text("zed {file}:{line}"),
        "future.flag": Bool(true),
        "future.count": Int(-9223372036854775808),
        "future.ratio": Float(1e300),
        "future.name": Text("\u{1F600} \"quoted\" \\ back"),
    },
)"#
    .to_owned()
}

fn rich_keymap() -> String {
    r#"(
    format_version: 1,
    bindings: {
        "view.toggle_grid": "Ctrl+G",
        "view.frame_selection": "Alt+F",
        "view.toggle_labels": "Shift+L",
        "gizmo.translate": "Ctrl+Alt+1",
        "layout.reset": "Ctrl+Alt+Shift+R",
        "edit.undo": "F9",
        "outliner.delete": "Ctrl+Delete",
        "future.command": "Ctrl+F12",
        "future.weird": "Hyper+Banana",
    },
)"#
    .to_owned()
}

/// A layout with nested splits, every built-in panel and an unknown id.
fn rich_layout() -> String {
    let ids: Vec<String> = editor_layout::BUILT_IN_PANELS
        .iter()
        .map(|p| format!("{:?}", p.id()))
        .collect();
    let half = ids.len() / 2;
    format!(
        r#"(
    format_version: 1,
    root: Split(
        axis: Horizontal,
        fraction: 0.25,
        first: Tabs(tabs: [{}], active: 1),
        second: Split(
            axis: Vertical,
            fraction: 0.7,
            first: Tabs(tabs: [{}, "ext.unknown"], active: 0),
            second: Tabs(tabs: ["viewport"], active: 0),
        ),
    ),
    known_panels: [{}],
)"#,
        ids[..half].join(", "),
        ids[half..].join(", "),
        ids.join(", ")
    )
}

/// A manifest with every table Project Settings models and some it does not.
fn rich_manifest() -> String {
    r#"[project]
name = "Monkey Yard"
id = "com.example.yard"
version = "1.2.0"
build_number = 7
engine = "0.2"
default_level = "levels/main.level.ron"

[platforms.android]
min_sdk = 29
target_sdk = 35
orientation = "sensor_landscape"
abis = ["arm64-v8a", "x86_64"]
render_profile = "MobileHigh"
keystore = "env:KEY"

[platforms.linux]
render_profile = "DesktopHigh"

[cook.chunks.base]
include = ["**"]

[cook.chunks.arena]
include = ["levels/arena/**"]
requires = ["base"]
delivery = "on_demand"
priority = 3

[content]
base_url = "https://example.com/content"

[steam]
app_id = 480

[mystery]
answer = 42
when = 1979-05-27T07:32:00Z
list = [1, 2.5, "three", { four = 4 }]
"#
    .to_owned()
}

// ------------------------------------------------------------------------------------------------
// Loaders
// ------------------------------------------------------------------------------------------------

/// The editor's own loader of the `editor.<name>` fuzz target (`baseline_fuzz`).
fn editor_loader(name: &str) -> fn(&str) -> Result<String, String> {
    for target in regia_editor::baseline_fuzz::targets() {
        if target.name == name
            && let FuzzInput::Text { load, .. } = target.input
        {
            return load;
        }
    }
    panic!("no editor fuzz target {name}");
}

/// `load`, then the strict property: its output loads again and re-encodes to the same bytes.
fn strict(load: fn(&str) -> Result<String, String>, text: &str) -> Result<String, String> {
    let first = load(text)?;
    let second = load(&first).map_err(|e| format!("{BUG_PREFIX}the written file does not read back: {e}"))?;
    if first != second {
        let at = first
            .bytes()
            .zip(second.bytes())
            .position(|(a, b)| a != b)
            .unwrap_or(first.len().min(second.len()));
        return Err(format!(
            "{BUG_PREFIX}write, read, write is not byte-identical at byte {at}: {:?} then {:?}",
            first.get(at.saturating_sub(30)..(at + 30).min(first.len())),
            second.get(at.saturating_sub(30)..(at + 30).min(second.len()))
        ));
    }
    Ok(first)
}

fn strict_prefs(text: &str) -> Result<String, String> {
    strict(editor_loader("editor.prefs"), text)
}

fn strict_keymap(text: &str) -> Result<String, String> {
    strict(editor_loader("editor.keymap"), text)
}

fn strict_layout(text: &str) -> Result<String, String> {
    strict(editor_loader("editor.layout"), text)
}

/// A temp project folder removed when its thread ends.
struct ScratchProject(PathBuf);

impl Drop for ScratchProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

thread_local! {
    /// The project folder the settings loader writes `Regia.toml` into (one per test thread).
    static SETTINGS_PROJECT: ScratchProject = ScratchProject(temp_dir("settings-loader"));
    /// The egui context the settings loader draws the panel in.
    static SETTINGS_CTX: egui::Context = egui::Context::default();
    /// How many accepted manifests drew the panel so far (every 8th draws: a frame costs more
    /// than a parse).
    static SETTINGS_DRAWS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Project Settings over manifest text: `ProjectSettingsState::load` of the file, every accessor
/// the panel and the agent surface use, sometimes a panel frame, and the bytes `save` writes.
fn settings_load(text: &str) -> Result<String, String> {
    let mut state = ProjectSettingsState::default();
    SETTINGS_PROJECT.with(|project| {
        std::fs::write(project.0.join("Regia.toml"), text)
            .map_err(|e| format!("cannot write the fixture: {e}"))?;
        state.load(&project.0)
    })?;
    let _ = state.chunk_rules();
    let _ = state.content_base_url();
    let _ = state.project_string("name");
    let _ = state.project_int("build_number");
    let _ = state.platform_targets();
    for os in regia_editor::project_settings_ext::PLATFORM_TARGETS {
        let _ = state.render_profile(os);
    }
    if let Some(root) = state.manifest() {
        let _ = regia_editor::project_settings_ext::problems(root);
    }
    let draw = SETTINGS_DRAWS.with(|n| {
        n.set(n.get() + 1);
        n.get().is_multiple_of(8)
    });
    if draw {
        SETTINGS_CTX.with(|ctx| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                regia_editor::project_settings::project_settings_panel(
                    ui,
                    &mut state,
                    Some(Path::new("/nonexistent")),
                );
            });
            output.textures_delta.clear();
        });
    }
    let root = state.manifest().cloned().unwrap_or_default();
    toml::to_string_pretty(&root).map_err(|e| format!("cannot serialize: {e}"))
}

fn strict_settings(text: &str) -> Result<String, String> {
    strict(settings_load, text)
}

/// The four strict targets with their rich fixtures.
fn targets() -> Vec<FuzzTarget> {
    let text =
        |name: &'static str, fixture: fn() -> String, load: fn(&str) -> Result<String, String>| FuzzTarget {
            name,
            format: None,
            format_version: 1,
            round_trip: true,
            input: FuzzInput::Text { fixture, load },
        };
    vec![
        text("editor.prefs (strict)", rich_prefs, strict_prefs),
        text("editor.keymap (strict)", rich_keymap, strict_keymap),
        text("editor.layout (strict)", rich_layout, strict_layout),
        text("editor.project_settings (strict)", rich_manifest, strict_settings),
    ]
}

// ------------------------------------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------------------------------------

#[test]
fn the_rich_fixtures_load_and_round_trip_byte_identically() {
    for target in targets() {
        let FuzzInput::Text { fixture, load } = target.input else {
            unreachable!("text targets")
        };
        let canonical =
            load(&fixture()).unwrap_or_else(|e| panic!("{}: the fixture does not load: {e}", target.name));
        assert_eq!(
            load(&canonical).as_deref(),
            Ok(canonical.as_str()),
            "{}",
            target.name
        );
    }
    // The fixtures exercise what they claim to.
    let (prefs, diagnostics) = EditorPreferences::from_text(&rich_prefs()).expect("prefs");
    assert!(!prefs.is_default("appearance.ui_scale"));
    assert_eq!(prefs.unknown_keys().len(), 4, "{diagnostics:?}");
    let keymap = editor_keymap::parse_keymap(&rich_keymap()).expect("keymap");
    assert!(keymap.keymap.bindings.len() >= 6, "{keymap:?}");
    assert_eq!(keymap.unknown.len(), 2);
    let layout = LayoutDoc::from_text(&rich_layout()).expect("layout");
    let (_, report) = layout.to_dock().expect("dock");
    // The unknown id and the repeated viewport are dropped (the repeat collapses its split).
    assert_eq!(
        report.dropped,
        vec!["ext.unknown".to_owned(), "viewport".to_owned()]
    );
}

#[test]
fn many_seeds_of_mutations_never_panic_and_round_trip_byte_identically() {
    for target in targets() {
        let mut accepted = 0;
        let mut rejected = 0;
        for seed in 0..SEEDS {
            let report = regia_agent::fuzz_targets::run(&target, None, 0xED17_0000 + seed, ITERATIONS)
                .unwrap_or_else(|e| panic!("{}: {e}", target.name));
            assert!(
                report.is_clean(),
                "{} seed {:#x}: {report:?}\nreplay: regia_agent::fuzz_targets::run(target, None, {:#x}, {ITERATIONS})",
                target.name,
                0xED17_0000 + seed,
                0xED17_0000 + seed
            );
            accepted += report.accepted;
            rejected += report.rejected;
        }
        assert!(
            accepted > 0 && rejected > 0,
            "{}: accepted {accepted}, rejected {rejected}",
            target.name
        );
    }
}

/// Inputs no editor ever wrote.
fn hostile_inputs() -> Vec<String> {
    let deep_parens = format!("{}{}", "(".repeat(100_000), ")".repeat(100_000));
    let deep_split = {
        let mut node = "Tabs(tabs: [\"viewport\"])".to_owned();
        for _ in 0..200 {
            node = format!(
                "Split(axis: Vertical, fraction: 0.5, first: {node}, second: Tabs(tabs: [\"outliner\"]))"
            );
        }
        format!("(format_version: 1, root: {node})")
    };
    let deep_toml = format!("a = {}1{}", "[".repeat(50_000), "]".repeat(50_000));
    let deep_inline = format!("a = {}1{}", "{ b = ".repeat(20_000), " }".repeat(20_000));
    let many_tabs = format!(
        "(format_version: 1, root: Tabs(tabs: [{}]))",
        vec!["\"viewport\""; 100_000].join(",")
    );
    vec![
        String::new(),
        "\u{FEFF}".to_owned(),
        "\u{FEFF}(format_version: 1)".to_owned(),
        "\0\0\0\0".to_owned(),
        "()".to_owned(),
        "(format_version: 0)".to_owned(),
        "(format_version: -1)".to_owned(),
        "(format_version: 4294967295)".to_owned(),
        "(format_version: 4294967296)".to_owned(),
        "(format_version: 2, values: 5)".to_owned(),
        "(format_version: 1, values: { \"appearance.ui_scale\": Float(NaN), \"camera.fly_speed\": Float(inf) })".to_owned(),
        "(format_version: 1, values: { \"autosave.interval_minutes\": Int(9223372036854775807) })".to_owned(),
        "(format_version: 1, values: { \"appearance.theme\": Int(3) })".to_owned(),
        "(format_version: 1, bindings: { \"edit.undo\": \"\", \"edit.redo\": \"Ctrl+\", \"view.toggle_grid\": \"Ctrl+Ctrl+Ctrl\" })"
            .to_owned(),
        "(format_version: 1, root: Tabs(tabs: [], active: 18446744073709551615))".to_owned(),
        "(format_version: 1, root: Split(axis: Horizontal, fraction: NaN, first: Tabs(tabs: [\"viewport\"]), second: Tabs(tabs: [\"viewport\"])))"
            .to_owned(),
        "(format_version: 1, root: Split(axis: Vertical, fraction: -inf, first: Tabs(tabs: [\"x\"]), second: Tabs(tabs: [\"y\"])))"
            .to_owned(),
        deep_parens,
        deep_split,
        many_tabs,
        deep_toml,
        deep_inline,
        "[project]\nname = 5\nid = []\n[platforms]\nandroid = 3\n[cook]\nchunks = \"all\"\n".to_owned(),
        "[project]\nname = \"\"\n[platforms.android]\nrender_profile = 7\nabis = [1, [2], { a = 3 }]\n".to_owned(),
        "project = 1\nplatforms = []\ncontent = { base_url = 4 }\n".to_owned(),
        "[[project]]\nname = \"a\"\n[[project]]\nname = \"b\"\n".to_owned(),
        "x".repeat(2 << 20),
        format!("(format_version: 1, values: {{ \"future\": Text(\"{}\") }})", "y".repeat(1 << 20)),
    ]
}

/// Every reader over `text`: a value or a non-empty error, never a panic.
fn read_all(text: &str) -> Vec<(&'static str, Result<(), String>)> {
    let mut out = Vec::new();
    let mut check = |name: &'static str, result: Result<(), String>| {
        if let Err(e) = &result {
            assert!(
                !e.trim().is_empty(),
                "{name}: empty error for {:?}",
                text.get(..80)
            );
        }
        out.push((name, result));
    };
    check(
        "prefs",
        EditorPreferences::from_text(text)
            .map(drop)
            .map_err(|e| e.to_string()),
    );
    check(
        "keymap",
        editor_keymap::parse_keymap(text)
            .map(drop)
            .map_err(|e| e.to_string()),
    );
    check(
        "layout",
        LayoutDoc::from_text(text)
            .and_then(|doc| doc.to_dock().map(drop))
            .map_err(|e| e.to_string()),
    );
    check("settings", settings_load(text).map(drop));
    out
}

#[test]
fn truncations_and_hostile_inputs_never_panic() {
    // Each fixture cut at every character boundary.
    for fixture in [rich_prefs(), rich_keymap(), rich_layout(), rich_manifest()] {
        for (cut, _) in fixture.char_indices() {
            let _ = read_all(&fixture[..cut]);
        }
    }
    for input in hostile_inputs() {
        let results = read_all(&input);
        // Nothing hostile is accepted as a layout (the editor would show it).
        let layout_ok = results.iter().any(|(n, r)| *n == "layout" && r.is_ok());
        assert!(
            !layout_ok || input.len() < 400,
            "hostile layout accepted: {:?}",
            input.get(..120)
        );
    }
}

/// Bytes on disk the text tests cannot express.
fn hostile_files() -> Vec<Vec<u8>> {
    let mut files: Vec<Vec<u8>> = hostile_inputs()
        .into_iter()
        .filter(|t| t.len() < 1 << 16)
        .map(String::into_bytes)
        .collect();
    files.push(vec![0xFF, 0xFE, 0x00, 0x28]);
    files.push(b"(format_version: 1, values: { \"a\": Text(\"\xC3\x28\") })".to_vec());
    files.push(rich_prefs().into_bytes()[..37].to_vec());
    files
}

fn temp_dir(label: &str) -> PathBuf {
    regia_test_utils::unique_temp_dir(&format!("editor-files-fuzz-{label}"))
}

/// The prefs and keymap of a world whose config dir holds `bytes` as both files.
fn load_from_disk(dir: &Path, bytes: &[u8]) -> World {
    std::fs::write(dir.join(editor_prefs::PREFS_FILE), bytes).expect("prefs file");
    std::fs::write(dir.join(editor_prefs::KEYMAP_FILE), bytes).expect("keymap file");
    std::fs::write(dir.join(editor_prefs::USER_LAYOUT_FILE), bytes).expect("layout file");
    let mut world = World::new();
    world.insert_resource(EditorConfigPaths::at(dir));
    editor_prefs::load_prefs(&mut world);
    editor_keymap::load_keymap(&mut world);
    world
}

#[test]
fn hostile_files_on_disk_load_the_defaults_and_record_the_error() {
    let dir = temp_dir("disk");
    for bytes in hostile_files() {
        let world = load_from_disk(&dir, &bytes);
        let shown = String::from_utf8_lossy(&bytes[..bytes.len().min(60)]).into_owned();
        // Preferences: a refused file leaves the defaults, with the reason recorded.
        let store = world.resource::<PrefsStore>();
        let prefs = world.resource::<EditorPreferences>();
        match EditorPreferences::from_text(std::str::from_utf8(&bytes).unwrap_or("\u{0}not utf-8")) {
            Ok(_) => {}
            Err(err) => {
                assert_eq!(prefs.generation(), 0, "{shown:?}: defaults");
                assert!(
                    store.read_only.is_some()
                        || store.diagnostics.iter().any(|d| d.contains("using the defaults")),
                    "{shown:?}: the error is recorded ({err}): {store:?}"
                );
            }
        }
        // Keymap: a refused file binds nothing, with the reason recorded.
        let keymap = world.resource::<regia_editor::command_palette::CommandKeymap>();
        let kstore = world.resource::<KeymapStore>();
        if editor_keymap::parse_keymap(std::str::from_utf8(&bytes).unwrap_or("\u{0}")).is_err() {
            assert!(keymap.bindings.is_empty(), "{shown:?}: no user bindings");
            assert!(
                kstore.read_only.is_some() || !kstore.problems.is_empty(),
                "{shown:?}: the error is recorded: {kstore:?}"
            );
        }
        // Layout: a refused file is not offered (the shell keeps the built-in layout).
        let user = dir.join(editor_prefs::USER_LAYOUT_FILE);
        if editor_layout::read_layout_file(&user).is_err() {
            assert!(
                editor_layout::saved_layout_for(None, Some(&user)).is_none(),
                "{shown:?}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The edit world the shell tests build (scene, editor resources, the `editor.*` surface), reading
/// its preferences, keymap and user layout from `config`.
fn shell_app(config: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.world_mut().insert_resource(EditorConfigPaths::at(config));
    editor_prefs::load_prefs(app.world_mut());
    editor_keymap::load_keymap(app.world_mut());
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

/// The shell over a project whose layout, user files and `Regia.toml` all hold `bytes`.
#[test]
fn the_shell_draws_over_hostile_project_and_user_files() {
    for (i, bytes) in hostile_files().into_iter().enumerate() {
        let root = temp_dir(&format!("shell-{i}"));
        let config = temp_dir(&format!("shell-config-{i}"));
        let _ = load_from_disk(&config, &bytes);
        let mut app = shell_app(&config);
        let world = app.world_mut();
        std::fs::create_dir_all(root.join(".regia")).expect(".regia");
        std::fs::write(editor_layout::project_layout_path(&root), &bytes).expect("project layout");
        std::fs::write(root.join("Regia.toml"), &bytes).expect("manifest");
        // The settings reader refuses what does not parse, with a message.
        let mut settings = ProjectSettingsState::default();
        if let Err(err) = settings.load(&root) {
            assert!(!err.trim().is_empty());
        }
        // The agent's settings read answers with an error code or a value, never a panic.
        world.insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
        if let Some((_, handler)) = extension.method("editor.project_settings_get") {
            let _ = handler(world, &serde_json::json!({}));
        }
        let mut shell = EditorShell::new(None, Some(root.clone()));
        let ctx = egui::Context::default();
        for frame in 0..3 {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
                )),
                time: Some(f64::from(frame) / 60.0),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| shell.ui(ui, world, None));
            output.textures_delta.clear();
        }
        if editor_layout::read_layout_file(&editor_layout::project_layout_path(&root)).is_err() {
            assert_eq!(
                shell.layout_source(),
                editor_layout::LayoutSource::Default,
                "a refused layout file leaves the built-in layout"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&config);
    }
}

#[test]
fn saved_files_round_trip_byte_identically_through_disk() {
    let dir = temp_dir("disk-round-trip");
    // Preferences: read, write, read, write.
    let (prefs, _) = EditorPreferences::from_text(&rich_prefs()).expect("prefs");
    let first = prefs.to_text();
    let (again, _) = EditorPreferences::from_text(&first).expect("re-read");
    assert_eq!(again.to_text(), first);
    // Keymap.
    let read = editor_keymap::parse_keymap(&rich_keymap()).expect("keymap");
    let first = editor_keymap::keymap_to_text(&read.keymap, &read.unknown);
    let again = editor_keymap::parse_keymap(&first).expect("re-read");
    assert_eq!(
        editor_keymap::keymap_to_text(&again.keymap, &again.unknown),
        first
    );
    // Layout through the file writer.
    let path = dir.join("layout.ron");
    let doc = LayoutDoc::from_text(&rich_layout()).expect("layout");
    editor_layout::write_layout_file(&path, &doc).expect("write");
    let first = std::fs::read(&path).expect("read");
    let back = editor_layout::read_layout_file(&path)
        .expect("read")
        .expect("present");
    editor_layout::write_layout_file(&path, &back).expect("rewrite");
    assert_eq!(std::fs::read(&path).expect("read"), first);
    // Regia.toml through Project Settings' own load and save.
    let project = temp_dir("settings-round-trip");
    std::fs::write(project.join("Regia.toml"), rich_manifest()).expect("manifest");
    let mut state = ProjectSettingsState::default();
    state.load(&project).expect("load");
    state.mark_dirty();
    state.save(&project).expect("save");
    let first = std::fs::read(project.join("Regia.toml")).expect("read");
    let mut again = ProjectSettingsState::default();
    again.load(&project).expect("reload");
    again.mark_dirty();
    again.save(&project).expect("resave");
    assert_eq!(std::fs::read(project.join("Regia.toml")).expect("read"), first);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&project);
}
