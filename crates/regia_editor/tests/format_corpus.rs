// SPDX-License-Identifier: MPL-2.0
//! The editor's own file formats against the old-bytes corpus (ADR-2743, ADR-7902): `editor-prefs`,
//! `editor-keymap`, `editor-layout`, `editor-window-state`, `inspector-pins`, `editor-profile`,
//! `editor-view-state`, `viewport-snap`, `saved-searches`, `editor-theme` and `editor-panel` in `crates/regia_agent/tests/fixtures/format-corpus/`.
//! `regia_editor` is desktop-only and not a dependency of `regia_agent` (`COVERED_ELSEWHERE`), so the
//! four checks run here: every fixture loads, the re-encoding reads back equal, migrating is
//! idempotent, and a copy patched to the next version is refused with an error naming the version.

#[path = "../../regia_agent/tests/format_corpus_support/mod.rs"]
mod format_corpus_support;

use regia_editor::editor_keymap::{EDITOR_KEYMAP_FORMAT_VERSION, keymap_to_text, parse_keymap};
use regia_editor::editor_layout::{EDITOR_LAYOUT_FORMAT_VERSION, LayoutDoc};
use regia_editor::editor_prefs::{EDITOR_PREFS_FORMAT_VERSION, EditorPreferences};
use regia_editor::feature_profiles::{FeatureProfile, PROFILE_FORMAT_VERSION};
use regia_editor::inspector::pinned::{INSPECTOR_PINS_FORMAT_VERSION, parse_pins, pins_text};
use regia_editor::saved_searches::SAVED_SEARCHES_FORMAT_VERSION;
use regia_editor::theme::EDITOR_THEME_FORMAT_VERSION;
use regia_editor::view_state::{VIEW_STATE_FORMAT_VERSION, ViewStateDoc};
use regia_editor::viewport_ops::{SNAP_STEPS_FORMAT_VERSION, parse_snap_steps, snap_steps_text};
use regia_editor::window_state::{WINDOW_STATE_FORMAT_VERSION, WindowStateFile};

fn text(bytes: &[u8]) -> Result<&str, String> {
    std::str::from_utf8(bytes).map_err(|e| e.to_string())
}

fn prefs(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let (prefs, _unknown) = EditorPreferences::from_text(text(bytes)?).map_err(|e| e.to_string())?;
    Ok(prefs.to_text().into_bytes())
}

fn keymap(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let read = parse_keymap(text(bytes)?).map_err(|e| e.to_string())?;
    Ok(keymap_to_text(&read.keymap, &read.unknown).into_bytes())
}

fn layout(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let doc = LayoutDoc::from_text(text(bytes)?).map_err(|e| e.to_string())?;
    Ok(doc.to_text().into_bytes())
}

fn window_state(bytes: &[u8]) -> Result<Vec<u8>, String> {
    Ok(WindowStateFile::from_text(text(bytes)?)?.to_text().into_bytes())
}

fn pins(bytes: &[u8]) -> Result<Vec<u8>, String> {
    Ok(pins_text(&parse_pins(text(bytes)?)?.pins).into_bytes())
}

fn profile(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let profile = FeatureProfile::from_text(text(bytes)?).map_err(|e| e.to_string())?;
    Ok(profile.to_text().into_bytes())
}

fn view_state(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let doc = ViewStateDoc::from_text(text(bytes)?).map_err(|e| e.to_string())?;
    Ok(doc.to_text().into_bytes())
}

fn snap(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let (distance, angle_deg, scale) = parse_snap_steps(text(bytes)?)?;
    Ok(snap_steps_text(distance, angle_deg, scale).into_bytes())
}

fn editor_theme(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let (name, tokens) =
        regia_editor::theme::theme_file_from_text(text(bytes)?).map_err(|e| e.to_string())?;
    Ok(regia_editor::theme::theme_file_to_text(&name, &tokens).into_bytes())
}

fn editor_panel(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let def = regia_editor::utility_panel::format::EditorPanelDef::from_text(text(bytes)?)
        .map_err(|e| e.to_string())?;
    Ok(def.to_text().into_bytes())
}

fn saved_searches(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let read = regia_editor::saved_searches::parse(text(bytes)?).map_err(|e| e.to_string())?;
    Ok(regia_editor::saved_searches::to_text(&read.searches, &read.favorites).into_bytes())
}

/// `(format, current version, loader)` for every editor format.
type Row = (&'static str, u32, format_corpus_support::Loader<'static>);

fn formats() -> Vec<Row> {
    let rows: [Row; 11] = [
        ("editor-prefs", EDITOR_PREFS_FORMAT_VERSION, &prefs),
        ("editor-keymap", EDITOR_KEYMAP_FORMAT_VERSION, &keymap),
        ("editor-layout", EDITOR_LAYOUT_FORMAT_VERSION, &layout),
        ("editor-window-state", WINDOW_STATE_FORMAT_VERSION, &window_state),
        ("inspector-pins", INSPECTOR_PINS_FORMAT_VERSION, &pins),
        ("editor-profile", PROFILE_FORMAT_VERSION, &profile),
        ("editor-view-state", VIEW_STATE_FORMAT_VERSION, &view_state),
        ("viewport-snap", SNAP_STEPS_FORMAT_VERSION, &snap),
        ("saved-searches", SAVED_SEARCHES_FORMAT_VERSION, &saved_searches),
        ("editor-theme", EDITOR_THEME_FORMAT_VERSION, &editor_theme),
        (
            "editor-panel",
            regia_editor::utility_panel::format::EDITOR_PANEL_FORMAT_VERSION,
            &editor_panel,
        ),
    ];
    rows.to_vec()
}

#[test]
fn every_editor_fixture_loads_round_trips_and_refuses_the_future() {
    let mut problems = Vec::new();
    for (format, current, load) in formats() {
        problems.extend(format_corpus_support::check_format(format, current, load));
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// The table `cargo xtask check-formats` reads names the same formats and versions as this test.
#[test]
fn the_corpus_formats_are_the_core_descriptors() {
    let tested: Vec<(&str, u32)> = formats().iter().map(|(f, v, _)| (*f, *v)).collect();
    for d in regia_core::editor_formats::EDITOR_FORMATS {
        assert!(
            tested.contains(&(d.name, d.version)),
            "{} v{}: no corpus check here",
            d.name,
            d.version
        );
    }
}
