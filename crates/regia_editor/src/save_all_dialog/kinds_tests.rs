// SPDX-License-Identifier: MPL-2.0
//! P-147: the Save all dialog (and `editor.save_all`) saves every kind it lists. One fixture per
//! document kind leaves a document of that kind open in its editor with unsaved edits; Save all
//! with no ids must save it (the file on disk changes) and the kind is no longer listed.

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use serde_json::{Value, json};

mod fixtures_a;
mod fixtures_b;
mod fixtures_c;

/// A fixture: leaves one document of its kind open in its editor with unsaved edits, its file in
/// the project (`root`), and returns that file's absolute path. The app has `ScenePlugin`, the
/// document autosave registry (with every built-in provider and save hook) and the open project.
pub(super) type Fixture = fn(&mut regia_app::App, &Path) -> PathBuf;

/// Every provider kind and its fixture.
fn fixtures() -> Vec<(&'static str, Fixture)> {
    let mut out = Vec::new();
    out.extend(fixtures_a::fixtures());
    out.extend(fixtures_b::fixtures());
    out.extend(fixtures_c::fixtures());
    out
}

fn app_for(label: &str) -> (regia_app::App, PathBuf) {
    let root = regia_test_utils::unique_temp_dir(label);
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let world = app.world_mut();
    crate::doc_autosave::install(world);
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    (app, root)
}

fn save_all(world: &mut World, params: Value) -> Value {
    let (_, handler) = super::methods().into_iter().next().expect("editor.save_all");
    handler(world, &params).unwrap_or_else(|e| panic!("editor.save_all: {e:?}"))
}

/// Every registered provider kind has a fixture here and a save hook (a new editor that registers
/// a provider must register its save hook too).
#[test]
fn every_provider_kind_has_a_save_hook_and_a_fixture() {
    let (mut app, root) = app_for("save_all_hooks");
    let registry = app
        .world_mut()
        .resource::<crate::doc_autosave::DocAutosaveRegistry>()
        .clone();
    let covered: Vec<&str> = fixtures().iter().map(|(kind, _)| *kind).collect();
    for kind in registry.kinds() {
        assert!(registry.save_hook(kind).is_some(), "{kind} has no save hook");
        assert!(covered.contains(&kind), "{kind} has no fixture in kinds_tests");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn save_all_saves_every_listed_kind() {
    let mut failures = Vec::new();
    for (kind, fixture) in fixtures() {
        let (mut app, root) = app_for(&format!("save_all_kind_{kind}"));
        let file = fixture(&mut app, &root);
        let before = std::fs::read(&file).ok();
        let world = app.world_mut();
        let listed = save_all(world, json!({ "list": true }));
        let row = listed["documents"]
            .as_array()
            .and_then(|docs| docs.iter().find(|doc| doc["kind"] == kind).cloned());
        let Some(row) = row else {
            failures.push(format!("{kind}: the fixture's document is not listed: {listed}"));
            let _ = std::fs::remove_dir_all(&root);
            continue;
        };
        if row["can_save"] != json!(true) {
            failures.push(format!("{kind}: listed but not savable: {row}"));
            let _ = std::fs::remove_dir_all(&root);
            continue;
        }
        let out = save_all(world, json!({}));
        let id = row["id"].clone();
        let saved = out["saved"].as_array().is_some_and(|ids| ids.contains(&id));
        let still_listed = out["documents"]
            .as_array()
            .is_some_and(|docs| docs.iter().any(|doc| doc["kind"] == kind));
        let after = std::fs::read(&file).ok();
        if !saved {
            failures.push(format!("{kind}: not saved: {out}"));
        } else if still_listed {
            failures.push(format!("{kind}: still dirty after the save: {out}"));
        } else if after.is_none() || after == before {
            failures.push(format!("{kind}: {} did not change on disk", file.display()));
        }
        let _ = std::fs::remove_dir_all(&root);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
