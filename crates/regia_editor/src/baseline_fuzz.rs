// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets of the editor's per-user and per-project files (ADR-2321): `editor.prefs`
//! (`editor_prefs.ron`), `editor.keymap` (`editor_keymap.ron`) and `editor.layout`
//! (`editor_layout.ron`); since ADR-7902 also `editor.window_state` (`editor_window_state.ron`),
//! `editor.inspector_pins` (`inspector_pins.ron`), `editor.profile` (`.editor_profile.ron`),
//! `editor.view_state` (`.regia/editor_view.ron`), `editor.snap_steps` (`.regia/viewport_snap.ron`), `editor.saved_searches` (`saved_searches.ron`), `editor.theme` (`themes/<name>.editor_theme.ron`) and `editor.panel` (`*.editorpanel.ron`, ADR-8189),
//! the fuzz coverage of those frozen formats (`COVERED_ELSEWHERE` in `regia_agent`'s fuzz test). Each loader parses, consumes (validates every value; builds the dock for a
//! layout) and re-encodes, so `fuzz.run` checks the round-trip property on them. The editor adds
//! them to the agent's `FuzzTargetExtensions` beside the importer targets (`crate::import_fuzz`).

use regia_agent::fuzz_targets::{FuzzInput, FuzzTarget, FuzzTargetExtensions};
use regia_ecs::prelude::*;

use crate::editor_keymap::{EDITOR_KEYMAP_FORMAT_VERSION, keymap_to_text, parse_keymap};
use crate::editor_layout::{EDITOR_LAYOUT_FORMAT_VERSION, LayoutDoc, default_dock};
use crate::editor_prefs::{EDITOR_PREFS_FORMAT_VERSION, EditorPreferences};

fn prefs_fixture() -> String {
    r#"(
    format_version: 1,
    values: {
        "appearance.theme": Text("regia-light"),
        "appearance.ui_scale": Float(1.25),
        "autosave.interval_minutes": Int(10),
        "camera.fly_speed": Float(12.5),
        "layout.save_on_exit": Bool(false),
        "future.setting": Bool(true),
    },
)"#
    .to_owned()
}

fn prefs_load(text: &str) -> Result<String, String> {
    let (prefs, _) = EditorPreferences::from_text(text).map_err(|e| e.to_string())?;
    Ok(prefs.to_text())
}

fn keymap_fixture() -> String {
    r#"(
    format_version: 1,
    bindings: {
        "view.toggle_grid": "Ctrl+G",
        "view.frame_selection": "Alt+F",
        "layout.reset": "Ctrl+Alt+Shift+R",
        "future.command": "F9",
    },
)"#
    .to_owned()
}

fn keymap_load(text: &str) -> Result<String, String> {
    let read = parse_keymap(text).map_err(|e| e.to_string())?;
    // Consume: the conflict scan over the result.
    let _ = crate::editor_keymap::conflicts(&read.keymap);
    Ok(keymap_to_text(&read.keymap, &read.unknown))
}

fn layout_fixture() -> String {
    let mut doc = LayoutDoc::from_dock(&default_dock());
    doc.floating.push(crate::editor_layout::FloatingWindow {
        tabs: vec!["tables".into(), "problems".into()],
        active: 1,
        rect: Some((40.0, 60.0, 480.0, 320.0)),
    });
    // Version 3 (ADR-8105 (a)): a second Outliner and its own search and columns.
    doc.floating.push(crate::editor_layout::FloatingWindow {
        tabs: vec!["outliner#1".into()],
        active: 0,
        rect: None,
    });
    doc.instances.insert(
        "outliner#1".into(),
        crate::editor_layout::PanelInstanceState {
            filter: "kind:light".into(),
            columns: vec!["visible_locked".into(), "tags".into()],
            ..Default::default()
        },
    );
    doc.to_text()
}

fn layout_load(text: &str) -> Result<String, String> {
    let doc = LayoutDoc::from_text(text).map_err(|e| e.to_string())?;
    // Consume: build the dock (a layout naming no panel is rejected here).
    doc.to_dock().map_err(|e| e.to_string())?;
    Ok(doc.to_text())
}

fn window_state_fixture() -> String {
    r#"(
    format_version: 1,
    projects: {
        "/home/user/games/arena": (
            monitor: Some("DP-1"),
            position: Some((120, 80)),
            size: (1600, 900),
            fullscreen: false,
            maximized: true,
            level: Some("assets/maps/arena.level.ron"),
            asset_tabs: ["assets/materials/rock.matgraph.ron"],
        ),
    },
    order: ["/home/user/games/arena"],
)"#
    .to_owned()
}

fn window_state_load(text: &str) -> Result<String, String> {
    Ok(crate::window_state::WindowStateFile::from_text(text)?.to_text())
}

fn pins_fixture() -> String {
    r#"(
    format_version: 1,
    pins: {
        "regia_scene::transform::Transform": ["translation", "scale"],
        "regia_render::light::PointLight": ["intensity"],
    },
)"#
    .to_owned()
}

fn pins_load(text: &str) -> Result<String, String> {
    let file = crate::inspector::pinned::parse_pins(text)?;
    Ok(crate::inspector::pinned::pins_text(&file.pins))
}

fn profile_fixture() -> String {
    r#"(
    format_version: 1,
    name: "Level art",
    hidden_areas: ["networking", "scripting"],
    hidden_panels: ["profiler", "ext.unknown"],
    hidden_kinds: ["future.kind"],
)"#
    .to_owned()
}

fn profile_load(text: &str) -> Result<String, String> {
    let profile = crate::feature_profiles::FeatureProfile::from_text(text).map_err(|e| e.to_string())?;
    Ok(profile.to_text())
}

fn view_state_fixture() -> String {
    r#"(
    format_version: 1,
    recent_levels: ["assets/maps/arena.level.ron"],
    levels: {
        "assets/maps/arena.level.ron": (
            camera: Some((target: (0.0, 1.0, 0.0), distance: 12.0, yaw: 0.6, pitch: -0.4, speed: 8.0, fov_deg: 60.0)),
            bookmarks: {
                1: (target: (4.0, 0.0, -2.0), distance: 6.0, yaw: 1.2, pitch: -0.3, speed: 8.0, fov_deg: 60.0),
            },
            locked: [],
        ),
    },
    ui_designer: Some((size: (1920.0, 1080.0), safe_area: (0.0, 0.0, 0.0, 0.0), theme: None)),
)"#
    .to_owned()
}

fn view_state_load(text: &str) -> Result<String, String> {
    let doc = crate::view_state::ViewStateDoc::from_text(text).map_err(|e| e.to_string())?;
    Ok(doc.to_text())
}

fn snap_fixture() -> String {
    "(format_version: 1, distance: 0.5, angle_deg: 15.0, scale: 0.1)".to_owned()
}

fn snap_load(text: &str) -> Result<String, String> {
    let (distance, angle_deg, scale) = crate::viewport_ops::parse_snap_steps(text)?;
    Ok(crate::viewport_ops::snap_steps_text(distance, angle_deg, scale))
}

fn saved_searches_fixture() -> String {
    r#"(
    format_version: 1,
    searches: [
        (name: "Lights", query: "e: kind:light", providers: []),
        (name: "Rock assets", query: "rock", providers: ["assets"]),
    ],
    favorites: ["view.toggle_grid"],
)"#
    .to_owned()
}

fn saved_searches_load(text: &str) -> Result<String, String> {
    let read = crate::saved_searches::parse(text).map_err(|e| e.to_string())?;
    Ok(crate::saved_searches::to_text(&read.searches, &read.favorites))
}

fn theme_fixture() -> String {
    r##"(
    format_version: 1,
    name: "Studio Night",
    tokens: {
        "flat": true,
        "colors": {
            "ground": "#101418",
            "signal": "#ff8800",
        },
    },
)"##
    .to_owned()
}

fn theme_load(text: &str) -> Result<String, String> {
    let (name, tokens) = crate::theme::theme_file_from_text(text).map_err(|e| e.to_string())?;
    Ok(crate::theme::theme_file_to_text(&name, &tokens))
}

fn editor_panel_fixture() -> String {
    crate::utility_panel::STARTER_FILES[0].1.to_owned()
}

fn editor_panel_load(text: &str) -> Result<String, String> {
    let def = crate::utility_panel::format::EditorPanelDef::from_text(text).map_err(|e| e.to_string())?;
    Ok(def.to_text())
}

/// A round-trip text target.
fn text_target(
    name: &'static str,
    format_version: u32,
    fixture: fn() -> String,
    load: fn(&str) -> Result<String, String>,
) -> FuzzTarget {
    FuzzTarget {
        name,
        format: None,
        format_version,
        round_trip: true,
        input: FuzzInput::Text { fixture, load },
    }
}

/// The eleven targets.
pub fn targets() -> [FuzzTarget; 11] {
    [
        FuzzTarget {
            name: "editor.prefs",
            format: None,
            format_version: EDITOR_PREFS_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: prefs_fixture,
                load: prefs_load,
            },
        },
        FuzzTarget {
            name: "editor.keymap",
            format: None,
            format_version: EDITOR_KEYMAP_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: keymap_fixture,
                load: keymap_load,
            },
        },
        FuzzTarget {
            name: "editor.layout",
            format: None,
            format_version: EDITOR_LAYOUT_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: layout_fixture,
                load: layout_load,
            },
        },
        text_target(
            "editor.window_state",
            crate::window_state::WINDOW_STATE_FORMAT_VERSION,
            window_state_fixture,
            window_state_load,
        ),
        text_target(
            "editor.inspector_pins",
            crate::inspector::pinned::INSPECTOR_PINS_FORMAT_VERSION,
            pins_fixture,
            pins_load,
        ),
        text_target(
            "editor.profile",
            crate::feature_profiles::PROFILE_FORMAT_VERSION,
            profile_fixture,
            profile_load,
        ),
        text_target(
            "editor.view_state",
            crate::view_state::VIEW_STATE_FORMAT_VERSION,
            view_state_fixture,
            view_state_load,
        ),
        text_target(
            "editor.snap_steps",
            crate::viewport_ops::SNAP_STEPS_FORMAT_VERSION,
            snap_fixture,
            snap_load,
        ),
        text_target(
            "editor.saved_searches",
            crate::saved_searches::SAVED_SEARCHES_FORMAT_VERSION,
            saved_searches_fixture,
            saved_searches_load,
        ),
        text_target(
            "editor.theme",
            crate::theme::EDITOR_THEME_FORMAT_VERSION,
            theme_fixture,
            theme_load,
        ),
        text_target(
            "editor.panel",
            crate::utility_panel::format::EDITOR_PANEL_FORMAT_VERSION,
            editor_panel_fixture,
            editor_panel_load,
        ),
    ]
}

/// Adds [`targets`] to the app's `FuzzTargetExtensions`.
pub fn install(world: &mut World) {
    let mut extensions = world.get_resource_or_init::<FuzzTargetExtensions>();
    for target in targets() {
        if let Err(e) = extensions.add(target) {
            tracing::warn!(target: "regia_editor", "fuzz target not added: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_baseline_targets_run_clean() {
        let mut world = World::new();
        install(&mut world);
        let extensions = world.resource::<FuzzTargetExtensions>();
        for t in targets() {
            let t = extensions.target(t.name).expect("added");
            let report = regia_agent::fuzz_targets::run(t, None, 7, 400).expect("fixture loads");
            assert!(report.is_clean(), "{}: {report:?}", t.name);
            assert!(report.accepted > 0, "{}: nothing loaded: {report:?}", t.name);
            assert!(report.rejected > 0, "{}: nothing rejected: {report:?}", t.name);
        }
    }
}
