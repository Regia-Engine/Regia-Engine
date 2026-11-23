// SPDX-License-Identifier: MPL-2.0
//! Content Chunks closure settings (CT-26, ADR-8543), headless: the content mode and per-chunk
//! `entry` roots are authored from Project Settings and `editor.content_chunks_set_rule`, previewed
//! before saving (off-thread, stale results rejected), checked as the cook checks them, undoable to
//! the exact file bytes, refused under a read-only agent profile, and the unshipped assets page
//! through `editor.content_chunks_unshipped` with a total and a cursor.
//!
//! The fixture is a cooked registry (no cooked files) plus a `Regia.toml`; the reference
//! assignment is the one `regia assets chunks` computes (`startup_roots` + `assign_with_roots`).

#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_agent::extension::AgentExtension;
use regia_agent::{AgentConfig, AgentPermissions, AgentPlugin, AgentSettings, LocalReply, call_local};
use regia_app::prelude::*;
use regia_asset::AssetId;
use regia_asset::registry::{AssetRegistry, RegistryEntry, cache_registry_path};
use regia_asset_pipeline::chunks::{ChunkRules, ContentMode, assign_with_roots, startup_roots};
use regia_core::id::Guid;
use regia_editor::closure_settings::{
    PreviewRequest, PreviewStatus, PreviewWorker, preview_json, save_checked,
};
use regia_editor::command::UndoStack;
use regia_editor::project_settings::{ChunkRuleEdit, ProjectSettingsState};
use serde_json::{Value, json};

/// Unshipped junk assets in the fixture (pagination).
const JUNK: u128 = 25;

/// The fixture manifest: comments and key order a re-serialization would not keep, a pinned base
/// rule, a `dlc` rule with `follow`, `overrides` and an unknown key, and `shared` in
/// `[cook.chunk_options]`.
const MANIFEST: &str = "# Hand-written; undo must give these bytes back.\n\
[project]\n\
name = \"closure\"\n\
id = \"com.example.closure\"\n\
version = \"1.0.0\"\n\
startup_level = \"levels/main.level.ron\"\n\
\n\
[cook.chunk_options]\n\
shared = \"base\"\n\
\n\
[cook.chunks.base]\n\
pin = true\n\
\n\
[cook.chunks.dlc]\n\
include = [\"dlc/*.mesh\"]   # the expansion\n\
follow = true\n\
overrides = true\n\
note = \"kept by every edit\"\n";

fn id(n: u128) -> AssetId {
    AssetId(Guid::from_u128(n))
}

/// A project with a cooked registry and [`MANIFEST`].
fn project(tag: &str) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(&format!("closure-settings-{tag}"));
    let platform = regia_asset_pipeline::host_platform().to_string();
    let cache = regia_asset::cooked_cache_dir(&root, &platform);
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::write(root.join("assets/readme.txt"), "raw file").unwrap();
    let rec = |n: u128, path: &str, deps: &[u128]| RegistryEntry {
        id: id(n),
        path: format!("assets/{path}"),
        type_path: "regia_asset::CpuMesh".into(),
        dependencies: deps.iter().map(|d| id(*d)).collect(),
        tags: Default::default(),
        size: 100 + u64::try_from(n).unwrap_or(0),
    };
    let mut entries = vec![
        rec(1, "levels/main.level.ron", &[2]),
        rec(2, "env/rock.mesh", &[]),
        rec(3, "dlc/boss.mesh", &[4]),
        rec(4, "dlc/boss_tex.png", &[]),
        rec(5, "ui/old_logo.png", &[]),
        rec(6, "spawn/wave.table", &[2]),
    ];
    for i in 0..JUNK {
        entries.push(rec(100 + i, &format!("junk/unused_{i:02}.png"), &[]));
    }
    let registry = AssetRegistry::from_entries(entries);
    registry.write_to(&cache_registry_path(&cache)).unwrap();
    write_atomic(&root.join("Regia.toml"), MANIFEST);
    root
}

fn write_atomic(path: &Path, text: &str) {
    let tmp = path.with_extension("fixture-tmp");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

fn manifest(root: &Path) -> String {
    std::fs::read_to_string(root.join("Regia.toml")).unwrap()
}

/// An edit world with the editor's agent methods, the undo stack and the project.
fn edit_app(root: &Path, read_only: bool) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(AgentSettings {
        project_dir: Some(root.to_path_buf()),
    });
    app.add_plugins(AgentPlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<regia_editor::selection::Selection>()
        .init_resource::<regia_editor::viewport::EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>();
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
    app.finish_plugins();
    app.update();
    app.world_mut().insert_resource(AgentPermissions {
        config: AgentConfig {
            read_only,
            rate_limit_per_second: u32::MAX,
            heavy_rate_limit_per_second: u32::MAX,
            ..Default::default()
        },
        source: None,
    });
    app
}

/// Calls a method through the agent dispatcher (permissions apply); the error code on failure.
fn call(app: &mut App, method: &str, params: Value) -> Result<Value, String> {
    match call_local(app.world_mut(), method, params, None) {
        LocalReply::Done(result) => result.map_err(|e| {
            e.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str)
                .map_or_else(|| e.message.clone(), str::to_owned)
        }),
        LocalReply::Pending(_) => panic!("{method} answers in the call"),
    }
}

/// The assignment `regia assets chunks` computes from the saved manifest: shipped path to chunk,
/// and the unshipped paths.
fn cli_assignment(root: &Path) -> (BTreeMap<String, String>, Vec<String>) {
    let platform = regia_asset_pipeline::host_platform().to_string();
    let cache = regia_asset::cooked_cache_dir(root, &platform);
    let registry = AssetRegistry::read_from(&cache_registry_path(&cache))
        .unwrap()
        .unwrap();
    let rules = ChunkRules::from_manifest_toml(&manifest(root)).unwrap();
    let roots = startup_roots(root, &registry, &rules).unwrap();
    let assignment = assign_with_roots(&registry, &rules, &roots).unwrap();
    let path = |id: &AssetId| registry.by_id(*id).unwrap().path.clone();
    let chunk_of = assignment
        .chunk_of
        .iter()
        .map(|(i, c)| (path(i), c.clone()))
        .collect();
    let mut unshipped: Vec<String> = assignment.unshipped.iter().map(path).collect();
    unshipped.sort();
    (chunk_of, unshipped)
}

/// The closure edit every test makes: closure mode, the boss as the expansion's entry root, the
/// spawn tables as a dynamic root of base.
fn closure_edit(state: &mut ProjectSettingsState) {
    state.set_content_mode(ContentMode::Closure);
    let mut rules = state.chunk_rules();
    let dlc = rules.iter_mut().find(|r| r.name == "dlc").unwrap();
    dlc.entry = vec!["dlc/boss.mesh".into()];
    state.set_chunk_rule(&dlc.clone());
    let base = rules.iter_mut().find(|r| r.name == "base").unwrap();
    base.entry = vec!["spawn/**".into()];
    state.set_chunk_rule(&base.clone());
}

#[test]
fn entry_roots_and_mode_round_trip_without_losing_pin_follow_overrides() {
    let root = project("round-trip");
    let mut state = ProjectSettingsState::default();
    state.load(&root).unwrap();
    assert_eq!(state.content_mode(), ContentMode::All);
    closure_edit(&mut state);
    let saved = save_checked(&mut state, &root, "", "test").unwrap();
    assert!(saved.verified, "a cooked project checks its roots");

    let mut fresh = ProjectSettingsState::default();
    fresh.load(&root).unwrap();
    assert_eq!(fresh.content_mode(), ContentMode::Closure);
    let dlc = fresh.chunk_rules().into_iter().find(|r| r.name == "dlc").unwrap();
    assert_eq!(dlc.entry, ["dlc/boss.mesh"]);
    assert_eq!(dlc.include, ["dlc/*.mesh"]);
    let rules = ChunkRules::from_manifest_toml(&manifest(&root)).unwrap();
    assert_eq!(rules.content, ContentMode::Closure);
    assert!(rules.rule("base").pin, "pin stays");
    assert!(
        rules.rule("dlc").follow && rules.rule("dlc").overrides,
        "follow and overrides stay"
    );
    assert_eq!(rules.rule("base").entry, ["spawn/**"]);
    let text = manifest(&root);
    assert!(text.contains("kept by every edit"), "unknown keys stay:\n{text}");
    assert!(
        text.contains("shared = \"base\""),
        "[cook.chunk_options] shared stays:\n{text}"
    );

    // Edit and remove through the agent setter: other keys survive both.
    let mut app = edit_app(&root, false);
    let out = call(
        &mut app,
        "editor.content_chunks_set_rule",
        json!({ "chunk": "dlc", "entry": ["dlc/**"] }),
    )
    .unwrap();
    assert_eq!(
        (out["verified"].clone(), out["content"].clone()),
        (json!(true), json!("closure"))
    );
    assert_eq!(
        ChunkRules::from_manifest_toml(&manifest(&root))
            .unwrap()
            .rule("dlc")
            .entry,
        ["dlc/**"]
    );
    call(
        &mut app,
        "editor.content_chunks_set_rule",
        json!({ "chunk": "dlc", "entry": [] }),
    )
    .unwrap();
    call(
        &mut app,
        "editor.content_chunks_set_rule",
        json!({ "chunk": "base", "entry": [] }),
    )
    .unwrap();
    // Closure mode with no root left but the startup level still saves (the level is a root).
    let rules = ChunkRules::from_manifest_toml(&manifest(&root)).unwrap();
    assert!(rules.rule("dlc").entry.is_empty() && rules.rule("dlc").overrides);
    call(
        &mut app,
        "editor.content_chunks_set_rule",
        json!({ "content": "all" }),
    )
    .unwrap();
    let text = manifest(&root);
    let rules = ChunkRules::from_manifest_toml(&text).unwrap();
    assert_eq!(rules.content, ContentMode::All);
    assert!(
        !text.contains("content ="),
        "all is the default and leaves no key:\n{text}"
    );
    assert!(text.contains("shared = \"base\""), "{text}");
    assert!(rules.rule("base").pin && rules.rule("dlc").follow && rules.rule("dlc").overrides);
    assert!(text.contains("kept by every edit"), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

/// `preview_json` without the input fingerprint (the human and agent edits serialize the same
/// table, but the comparison should not depend on key order).
fn comparable(mut preview: Value) -> Value {
    if let Some(map) = preview.as_object_mut() {
        map.remove("fingerprint");
    }
    preview
}

#[test]
fn human_and_agent_previews_match_the_cli_assignment() {
    let root = project("parity");
    let mut app = edit_app(&root, false);
    // Base's dynamic root first (in `all` mode; it is checked and saves).
    let out = call(
        &mut app,
        "editor.content_chunks_set_rule",
        json!({ "chunk": "base", "entry": ["spawn/**"] }),
    )
    .unwrap();
    assert_eq!(out["verified"], true);
    let before = manifest(&root);

    // Human: the Project Settings state, unsaved, previewed on the worker thread.
    let mut state = ProjectSettingsState::default();
    state.load(&root).unwrap();
    closure_edit(&mut state);
    let mut worker = PreviewWorker::default();
    worker.request(PreviewRequest {
        project: root.clone(),
        platform: String::new(),
        edited: state.manifest_text().unwrap(),
    });
    assert_eq!(worker.wait(), PreviewStatus::Ready, "{:?}", worker.error());
    let human = worker.latest().unwrap().clone();
    assert_eq!(manifest(&root), before, "a preview writes nothing");
    assert!(human.valid);
    assert!(human.unshipped.contains(&"assets/ui/old_logo.png".to_owned()));
    assert_eq!(
        human.chunk_of.get("assets/dlc/boss_tex.png").map(String::as_str),
        Some("dlc")
    );
    assert_eq!(
        human.reason_chain("dlc/boss_tex.png"),
        [
            "assets/dlc/boss_tex.png",
            "assets/dlc/boss.mesh",
            "[cook.chunks.dlc] entry[0]"
        ],
        "the reason chain ends at the declaration that ships it"
    );
    assert!(
        human
            .moves
            .iter()
            .any(|m| m.path == "assets/ui/old_logo.png" && m.to.is_none()),
        "the preview names what stops shipping"
    );
    let human_json = comparable(preview_json(&human, 100));

    // Agent: the same edit in one preview call gives the same answer.
    let edit = json!({ "content": "closure", "chunk": "dlc", "entry": ["dlc/boss.mesh"] });
    let agent = call(&mut app, "editor.content_chunks_preview", edit.clone()).unwrap();
    assert_eq!(comparable(agent), human_json);
    assert_eq!(manifest(&root), before, "a preview writes nothing");

    // Saved, both match what `regia assets chunks` computes from the file.
    let saved = call(&mut app, "editor.content_chunks_set_rule", edit).unwrap();
    assert_eq!(comparable(saved["preview"].clone()), human_json);
    let (cli_chunks, cli_unshipped) = cli_assignment(&root);
    assert_eq!(
        human.chunk_of, cli_chunks,
        "the human preview is the CLI assignment"
    );
    assert_eq!(human.unshipped, cli_unshipped);
    // After the save, a preview of "no change" is the same assignment with no moves.
    let now = call(&mut app, "editor.content_chunks_preview", json!({})).unwrap();
    assert_eq!(now["moves_total"], 0);
    for key in [
        "shipped",
        "chunks",
        "unshipped_total",
        "unshipped",
        "roots",
        "unmatched",
        "valid",
    ] {
        assert_eq!(human_json[key], now[key], "{key}");
    }
    let chain = call(
        &mut app,
        "editor.content_chunks_preview",
        json!({ "asset": "spawn/wave.table" }),
    )
    .unwrap();
    assert_eq!(
        chain["chain"],
        json!(["assets/spawn/wave.table", "[cook.chunks.base] entry[0]"])
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unmatched_root_cannot_save_a_misleading_valid_state() {
    let root = project("unmatched");
    let before = manifest(&root);
    let mut state = ProjectSettingsState::default();
    state.load(&root).unwrap();
    closure_edit(&mut state);
    let mut dlc = state.chunk_rules().into_iter().find(|r| r.name == "dlc").unwrap();
    dlc.entry = vec!["dlc/missing/**".into()];
    state.set_chunk_rule(&dlc);
    let err = save_checked(&mut state, &root, "", "test").unwrap_err();
    assert_eq!(err.code, "content_chunks_entry_unmatched", "{}", err.message);
    assert_eq!(manifest(&root), before, "nothing written");

    let mut app = edit_app(&root, false);
    let depth = app.world().resource::<UndoStack>().undo_depth();
    let preview = call(
        &mut app,
        "editor.content_chunks_preview",
        json!({ "content": "closure", "chunk": "dlc", "entry": ["dlc/missing/**"] }),
    )
    .unwrap();
    assert_eq!(preview["valid"], false);
    assert_eq!(preview["error_code"], "content_chunks_entry_unmatched");
    assert_eq!(preview["unmatched"], json!(["[cook.chunks.dlc] entry[0]"]));
    for params in [
        json!({ "content": "closure", "chunk": "dlc", "entry": ["dlc/missing/**"] }),
        // `all` mode checks entry roots too: a root that matches nothing is never "valid".
        json!({ "chunk": "dlc", "entry": ["dlc/missing/**"] }),
    ] {
        let code = call(&mut app, "editor.content_chunks_set_rule", params.clone()).unwrap_err();
        assert_eq!(code, "content_chunks_entry_unmatched", "{params}");
    }
    assert_eq!(manifest(&root), before, "nothing written");
    assert_eq!(
        app.world().resource::<UndoStack>().undo_depth(),
        depth,
        "no undo entry"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn unshipped_pages_visit_each_excluded_asset_exactly_once() {
    let root = project("pages");
    let mut app = edit_app(&root, false);
    call(
        &mut app,
        "editor.content_chunks_set_rule",
        json!({ "content": "closure", "chunk": "dlc", "entry": ["dlc/boss.mesh"] }),
    )
    .unwrap();
    let (_, cli_unshipped) = cli_assignment(&root);
    assert!(cli_unshipped.len() > 7 * 3, "the fixture needs several pages");
    let mut seen = Vec::new();
    let mut cursor = Value::Null;
    let mut pages = 0;
    loop {
        let mut params = json!({ "limit": 7 });
        if !cursor.is_null() {
            params["cursor"] = cursor.clone();
        }
        let page = call(&mut app, "editor.content_chunks_unshipped", params).unwrap();
        assert_eq!(
            page["total"],
            json!(cli_unshipped.len()),
            "the total is every unshipped asset"
        );
        assert_eq!(page["content"], "closure");
        for asset in page["assets"].as_array().unwrap() {
            assert_eq!(asset["reason"], "unreached");
            seen.push(asset["path"].as_str().unwrap().to_owned());
        }
        pages += 1;
        cursor = page["next_cursor"].clone();
        if cursor.is_null() {
            break;
        }
    }
    assert_eq!(pages, cli_unshipped.len().div_ceil(7));
    let unique: BTreeSet<&String> = seen.iter().collect();
    assert_eq!(unique.len(), seen.len(), "no asset on two pages");
    assert_eq!(seen, cli_unshipped, "every unshipped asset, in path order");

    let junk = call(
        &mut app,
        "editor.content_chunks_unshipped",
        json!({ "filter": "junk/*", "limit": 10 }),
    )
    .unwrap();
    assert_eq!(junk["matched"], json!(JUNK));
    assert_eq!(junk["assets"].as_array().map(Vec::len), Some(10));
    let logo = call(
        &mut app,
        "editor.content_chunks_unshipped",
        json!({ "filter": "logo" }),
    )
    .unwrap();
    assert_eq!(logo["assets"][0]["path"], "assets/ui/old_logo.png");
    assert!(logo["next_cursor"].is_null());
    // The existing references view opens an unshipped asset (its chunk is empty).
    let refs = call(
        &mut app,
        "editor.content_chunks_references",
        json!({ "asset": "ui/old_logo.png" }),
    )
    .unwrap();
    assert_eq!(refs["chunk"], "");
    assert_eq!(
        call(&mut app, "editor.content_chunks_unshipped", json!({ "limit": 0 })).unwrap_err(),
        "F0002"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_source_or_config_change_invalidates_a_pending_preview() {
    let root = project("stale");
    let mut state = ProjectSettingsState::default();
    state.load(&root).unwrap();
    closure_edit(&mut state);
    let edited = state.manifest_text().unwrap();
    let request = || PreviewRequest {
        project: root.clone(),
        platform: String::new(),
        edited: edited.clone(),
    };
    let mut worker = PreviewWorker::default();

    // A config change after the request: the result is discarded.
    worker.request(request());
    write_atomic(&root.join("Regia.toml"), &format!("{MANIFEST}# touched\n"));
    assert_eq!(worker.wait(), PreviewStatus::Stale);
    assert_eq!(
        worker.error().map(|e| e.code),
        Some("content_chunks_preview_stale")
    );
    assert!(worker.latest().is_none(), "a stale preview is never shown");

    // Unchanged inputs: accepted.
    worker.request(request());
    assert_eq!(worker.wait(), PreviewStatus::Ready);

    // A source change (a new raw file under assets/): discarded, and the old preview is cleared.
    worker.request(request());
    std::fs::write(root.join("assets/levels.txt"), "new source").unwrap();
    assert_eq!(worker.wait(), PreviewStatus::Stale);
    assert!(worker.latest().is_none());

    // Requests made while one computes are batched: the newest waits, the middle one is dropped,
    // the in-flight result is superseded, so three requests cost two computations.
    let start = worker.counters;
    worker.request(request());
    worker.request(request());
    worker.request(request());
    assert_eq!(worker.wait(), PreviewStatus::Ready);
    let c = worker.counters;
    assert_eq!(c.requested - start.requested, 3);
    assert_eq!(c.computed - start.computed, 2);
    assert_eq!(c.coalesced - start.coalesced, 1);
    assert_eq!(c.superseded - start.superseded, 1);
    assert_eq!(c.accepted - start.accepted, 1);
    assert_eq!(c.stale, 2);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_read_only_profile_refuses_settings_mutations() {
    let root = project("read-only");
    let before = manifest(&root);
    let mut app = edit_app(&root, true);
    for params in [
        json!({ "content": "closure", "chunk": "dlc", "entry": ["dlc/boss.mesh"] }),
        json!({ "chunk": "dlc", "remove": true }),
    ] {
        assert_eq!(
            call(&mut app, "editor.content_chunks_set_rule", params.clone()).unwrap_err(),
            "F0012",
            "{params}"
        );
    }
    assert_eq!(manifest(&root), before, "nothing written");
    assert_eq!(app.world().resource::<UndoStack>().undo_depth(), 0);
    // Reading still works: the preview and the pages.
    let preview = call(
        &mut app,
        "editor.content_chunks_preview",
        json!({ "content": "closure", "chunk": "dlc", "entry": ["dlc/boss.mesh"] }),
    )
    .unwrap();
    assert_eq!(preview["valid"], true);
    assert_eq!(manifest(&root), before, "the preview writes nothing");
    call(&mut app, "editor.content_chunks_unshipped", json!({})).unwrap();
    // The schema says so too.
    let extension = app.world().resource::<AgentExtension>().clone();
    let (schema, _) = extension.method("editor.content_chunks_set_rule").unwrap();
    assert!(schema.mutating);
    for read in ["editor.content_chunks_preview", "editor.content_chunks_unshipped"] {
        assert!(!extension.method(read).unwrap().0.mutating, "{read}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn undo_restores_the_exact_settings() {
    let root = project("undo");
    let before = manifest(&root);
    let mut app = edit_app(&root, false);
    call(
        &mut app,
        "editor.content_chunks_set_rule",
        json!({ "content": "closure", "chunk": "dlc", "entry": ["dlc/boss.mesh"] }),
    )
    .unwrap();
    let after = manifest(&root);
    assert_ne!(after, before);
    let history = app.world().resource::<UndoStack>().history();
    let last = history.last().unwrap();
    assert!(
        last.description.contains("content chunk settings (agent)"),
        "{}",
        last.description
    );
    assert_eq!(last.documents, ["Regia.toml"], "provenance names the manifest");
    assert!(regia_editor::command::undo_world(app.world_mut()));
    assert_eq!(
        manifest(&root),
        before,
        "undo gives back the exact bytes, comments included"
    );
    let state = call(&mut app, "editor.content_chunks_state", json!({})).unwrap();
    assert_eq!(state["content"], "all", "the panel recomputes after the undo");
    assert!(regia_editor::command::redo_world(app.world_mut()));
    assert_eq!(manifest(&root), after);

    // A Project Settings save makes the same kind of entry.
    let mut settings = ProjectSettingsState::default();
    settings.load(&root).unwrap();
    settings.set_chunk_rule(&ChunkRuleEdit {
        name: "extra".into(),
        include: vec!["junk/**".into()],
        ..ChunkRuleEdit::default()
    });
    let saved = save_checked(&mut settings, &root, "", "Project Settings").unwrap();
    regia_editor::closure_settings::push_manifest_undo(app.world_mut(), saved.undo);
    assert!(regia_editor::command::undo_world(app.world_mut()));
    assert_eq!(manifest(&root), after);
    let _ = std::fs::remove_dir_all(&root);
}

/// The agent workflow: an agent edits the settings, then waits on the published state with
/// `play.wait_until` and checks it with `play.expect` (fixed frames, no sleeps).
#[test]
fn agents_verify_the_settings_with_wait_until_and_expect() {
    let root = project("agent-flow");
    let mut app = edit_app(&root, false);
    let out = call(
        &mut app,
        "editor.content_chunks_set_rule",
        json!({ "content": "closure", "chunk": "dlc", "entry": ["dlc/boss.mesh"] }),
    )
    .unwrap();
    let preview = out["preview"].clone();
    let frame = app.world().resource::<Time>().frame_count();
    app.world_mut()
        .resource_mut::<regia_agent::AgentEventLog>()
        .push_host(frame, "content_chunks_preview", preview);
    let condition = json!({"all": [
        {"events_contain": {"kind": "content_chunks_preview", "contains": "[cook.chunks.dlc] entry[0]"}},
        {"events_contain": {"kind": "content_chunks_preview", "contains": "assets/ui/old_logo.png"}},
    ]});
    let pending = call_local(
        app.world_mut(),
        "play.wait_until",
        json!({"condition": condition, "timeout_ms": 10_000}),
        None,
    );
    for _ in 0..2 {
        app.update();
    }
    let response = match pending {
        LocalReply::Pending(receiver) => receiver.try_recv().expect("wait reply after fixed updates"),
        LocalReply::Done(other) => panic!("wait should defer: {other:?}"),
    };
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(
        response.result.as_ref().unwrap_or(&Value::Null)["satisfied"],
        true
    );
    match call_local(
        app.world_mut(),
        "play.expect",
        json!({"assertions": [{"name": "closure root and intentional unshipped asset", "condition": condition}]}),
        None,
    ) {
        LocalReply::Done(Ok(value)) => assert_eq!(value["pass"], true, "{value}"),
        LocalReply::Done(Err(e)) => panic!("expect: {e:?}"),
        LocalReply::Pending(_) => panic!("expect answers in the call"),
    }
    let _ = std::fs::remove_dir_all(&root);
}
