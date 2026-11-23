// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets of the editor's state files that had none (w11 item 5): `editor.view_state`
//! (`.regia/editor_view.ron`, [`crate::view_state`]), `editor.autosave_index` (the document
//! autosave index `.regia/autosave/docs/index.ron`, [`crate::doc_autosave`]) and
//! `editor.tool_manifest` (`<id>.tool.toml`, [`crate::tools::manifest`]). Each loader is the one
//! the editor reads the file with, then consumes the value as the editor does; the two RON files
//! re-encode, so `fuzz.run` checks the round-trip property on them (a tool manifest has no writer).
//! The editor adds them to the agent's `FuzzTargetExtensions` beside the importer targets
//! ([`crate::import_fuzz`]) and the preference files ([`crate::baseline_fuzz`]).

use regia_agent::fuzz_targets::{FuzzInput, FuzzTarget, FuzzTargetExtensions};
use regia_ecs::prelude::*;

use crate::doc_autosave::{INDEX_VERSION, index_text, parse_index};
use crate::tools::manifest::{MANIFEST_FORMAT, ToolManifest};
use crate::view_state::{VIEW_STATE_FORMAT_VERSION, ViewStateDoc};

fn view_state_fixture() -> String {
    r#"(
    format_version: 1,
    recent_levels: ["assets/levels/arena.level.ron", "assets/levels/hub.level.ron"],
    levels: {
        "assets/levels/arena.level.ron": (
            camera: Some((target: (1.0, 2.0, 3.0), distance: 12.5, yaw: 0.75, pitch: -0.3, speed: 8.0, fov_deg: 60.0)),
            bookmarks: {
                1: (target: (0.0, 0.0, 0.0), distance: 4.0, yaw: 0.0, pitch: 0.0, speed: 5.0, fov_deg: 45.0),
            },
            locked: ["00000000-0000-0000-0000-00000000002a"],
        ),
    },
)"#
    .to_owned()
}

fn view_state_load(text: &str) -> Result<String, String> {
    let doc = ViewStateDoc::from_text(text).map_err(|e| format!("{}: {e}", e.code()))?;
    // Consume as the editor does: every pose is checked before it moves a camera.
    for view in doc.levels.values() {
        let _ = view
            .camera
            .as_ref()
            .and_then(crate::view_state::CameraPose::camera);
        for pose in view.bookmarks.values() {
            let _ = pose.camera();
        }
    }
    Ok(doc.to_text())
}

fn autosave_index_fixture() -> String {
    r#"(
    version: 1,
    docs: [
        (
            id: "0123456789abcdef",
            kind: "matgraph",
            original: Some("assets/materials/rock.matgraph.ron"),
            label: "rock.matgraph.ron",
            file: "0123456789abcdef.autosave",
            size: 2048,
            saved_unix_ms: 1763164800000,
        ),
        (
            id: "fedcba9876543210",
            kind: "ui_design",
            original: None,
            label: "untitled ui_design",
            file: "fedcba9876543210.autosave",
            size: 17,
            saved_unix_ms: 1763164801000,
        ),
    ],
)"#
    .to_owned()
}

fn autosave_index_load(text: &str) -> Result<String, String> {
    let (index, _dropped) = parse_index(text)?;
    index_text(&index)
}

fn tool_manifest_fixture() -> String {
    r#"format = 1
id = "level-notes"
name = "Level Notes"
version = "0.2.0"
description = "Notes pinned to entities."

[launch]
program = "level-notes"
args = ["--editor-tool"]
autostart = false
env = { LEVEL_NOTES_PANEL = "1" }

[scope]
permissions = ["read", "edit_scene"]
methods = ["scene.*", "selection.get"]

[[provides]]
name = "notes.list"
description = "Every note of the open level."

[[provides]]
name = "notes.add"
permission = "edit_scene"
params = [{ name = "entity", kind = "string", required = true, description = "Entity guid." }]

[panel]
title = "Notes"
url = "http://127.0.0.1:8790/panel"
"#
    .to_owned()
}

fn tool_manifest_load(text: &str) -> Result<String, String> {
    let manifest = ToolManifest::parse(text)?;
    // Consume as the tool host does: the scope, every provided method's row, the panel title.
    let _ = manifest.scope_permissions();
    for method in &manifest.provides {
        let _ = method.to_json(&manifest.id);
    }
    let _ = manifest.title();
    // No writer: nothing to re-encode.
    Ok(String::new())
}

/// The three targets.
#[must_use]
pub fn targets() -> [FuzzTarget; 3] {
    [
        FuzzTarget {
            name: "editor.view_state",
            format: None,
            format_version: VIEW_STATE_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: view_state_fixture,
                load: view_state_load,
            },
        },
        FuzzTarget {
            name: "editor.autosave_index",
            format: None,
            format_version: INDEX_VERSION,
            round_trip: true,
            input: FuzzInput::Text {
                fixture: autosave_index_fixture,
                load: autosave_index_load,
            },
        },
        FuzzTarget {
            name: "editor.tool_manifest",
            format: None,
            format_version: MANIFEST_FORMAT,
            round_trip: false,
            input: FuzzInput::Text {
                fixture: tool_manifest_fixture,
                load: tool_manifest_load,
            },
        },
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

    /// Acceptance (w11 item 5): a short mutation run per target finds no panic, no empty error
    /// and no unstable re-encoding, and both loads and rejections happen.
    #[test]
    fn the_editor_state_targets_run_clean() {
        let mut world = World::new();
        install(&mut world);
        let extensions = world.resource::<FuzzTargetExtensions>();
        for t in targets() {
            let t = extensions.target(t.name).expect("added");
            for seed in 0..4 {
                let report = regia_agent::fuzz_targets::run(t, None, seed, 400).expect("fixture loads");
                assert!(report.is_clean(), "{} seed {seed}: {report:?}", t.name);
                assert!(report.accepted > 0, "{}: nothing loaded: {report:?}", t.name);
                assert!(report.rejected > 0, "{}: nothing rejected: {report:?}", t.name);
            }
        }
    }

    /// Malformed input degrades with the documented message, never a panic.
    #[test]
    fn malformed_editor_state_files_say_why() {
        let view = view_state_load("(format_version: 9)").expect_err("newer");
        assert!(view.starts_with("view_state_newer: "), "{view}");
        let view = view_state_load("not ron").expect_err("garbage");
        assert!(view.starts_with("view_state_parse: "), "{view}");

        let index = autosave_index_load("(version: 2, docs: [])").expect_err("newer");
        assert_eq!(index, "index version 2 is not 1");
        let index = autosave_index_load("[").expect_err("garbage");
        assert!(index.starts_with("not an autosave index: "), "{index}");
        // A row naming a path outside the docs folder is dropped, not loaded.
        let (kept, dropped) = parse_index(
            r#"(version: 1, docs: [(id: "../../../etc", kind: "k", original: None, label: "l",
                file: "x.autosave", size: 1, saved_unix_ms: 0)])"#,
        )
        .expect("parses");
        assert_eq!((kept.docs.len(), dropped), (0, 1));

        let tool = tool_manifest_load("id = \"x\"\n[launch]\nprogram = \"p\"\n").expect_err("no format");
        assert_eq!(tool, "`format = 1` is missing");
        let tool = tool_manifest_load("format = 1\nid = \"x\"\n[launch]\nprogram = \"p\"\nnew_key = 1\n")
            .expect_err("unknown key");
        assert!(tool.starts_with("invalid manifest: "), "{tool}");
        assert!(
            tool_manifest_load("[[[")
                .expect_err("garbage")
                .starts_with("TOML parse error: ")
        );
    }
}
