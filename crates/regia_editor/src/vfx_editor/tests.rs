// SPDX-License-Identifier: MPL-2.0
//! VFX editor tests: module listing, edits against hand edits, curve and gradient keys, undo to the
//! opened bytes, refused edits, the A/B toggle, cost rows, the agent surface, headless drawing and
//! (where a GPU adapter exists, lavapipe on the build host) the live preview.

use std::path::{Path, PathBuf};

use regia_vfx::format::{ColorKey, FloatKey, VfxSystemDef, VortexDef};
use serde_json::{Value, json};

use super::agent::methods;
use super::modules::{ModuleData, ModuleKind, present_kinds};
use super::sim::{emitter_costs, sim_hash};
use super::*;
use crate::data_panels::test_ui::{draw_frames, has_color};
use crate::viewport_agent::Handler;

const REL: &str = "vfx/fixture.vfx.ron";

/// A hand-written effect (not in the serializer's layout): three emitters over most module kinds.
const HAND: &str = r##"// editor fixture
(version: 7, name: "fixture", system: (budget: 8000),
 emitters: [
  (name: "fountain", spawn: (mode: Rate(60.0), lifetime: (min: 0.8, max: 1.2)),
   init: (shape: Cone(angle: 20.0, length: (min: 0.0, max: 0.1)),
          velocity: (mode: Radial(speed: (min: 3.0, max: 4.0))), size: (min: 0.1, max: 0.2),
          color: [(t: 0.0, color: "#ffcc88")]),
   update: (gravity: (0.0, -6.0, 0.0), drag: 0.5,
            size_over_life: [(t: 0.0, v: 1.0), (t: 1.0, v: 2.0)],
            color_over_life: [(t: 0.0, color: "#ffffff"), (t: 1.0, color: "#ff4000")]),
   render: (mode: Billboard, blend: Additive, sprite: SoftDisc)),
  (name: "burst", spawn: (mode: Burst(40), lifetime: (min: 0.5, max: 1.0)),
   init: (shape: Sphere(radius: (min: 0.0, max: 0.3)), velocity: (mode: Radial(speed: (min: 2.0, max: 5.0))),
          size: (min: 0.05, max: 0.1)),
   update: (gravity: (0.0, -9.8, 0.0), alpha_over_life: [(t: 0.0, v: 1.0), (t: 1.0, v: 0.0)], kill_below: Some(0.0)),
   render: (mode: Stretched, blend: Additive, sprite: Spark)),
  (name: "smoke", spawn: (mode: Rate(12.0), lifetime: (min: 2.0, max: 3.0)),
   init: (shape: Box(half_extents: (0.3, 0.1, 0.3)), velocity: (mode: Directional(dir: (0.0, 0.0, 1.0), spread: 10.0, speed: (min: 0.5, max: 1.0))),
          size: (min: 0.4, max: 0.6)),
   update: (curl: Some((0.5, 0.4, 1.0)), wind_response: 0.4),
   render: (mode: Billboard, blend: Alpha, sprite: Puff, softness: 0.5,
            flipbook: Some((cols: 4, rows: 4, frames: 16, mode: OverLife)))),
 ],
)
"##;

fn method(name: &str) -> Handler {
    methods()
        .into_iter()
        .find(|(m, _)| m.name == name)
        .map(|(_, h)| h)
        .expect("registered")
}

fn call(world: &mut World, name: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    method(name)(world, &params)
}

fn reason(e: &regia_agent::protocol::RpcError) -> String {
    e.data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// A world with a project that holds `text` at [`REL`].
fn project(text: &str) -> (World, PathBuf) {
    let root = regia_test_utils::unique_temp_dir("vfx-editor");
    let file = root.join(REL);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&file, text).expect("write");
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject { root: Some(root) });
    world.insert_resource(UndoStack::new());
    (world, file)
}

fn read(file: &Path) -> String {
    std::fs::read_to_string(file).expect("read")
}

fn def_of(world: &World) -> VfxSystemDef {
    world
        .get_resource::<VfxEditorPanel>()
        .and_then(VfxEditorPanel::doc)
        .map(|d| d.state.def.clone())
        .expect("open")
}

fn undo_depth(world: &World) -> usize {
    world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth)
}

fn listed(state: &Value, emitter: usize) -> Vec<String> {
    state["emitters"][emitter]["modules"]
        .as_array()
        .expect("modules")
        .iter()
        .map(|m| m["kind"].as_str().expect("kind").to_owned())
        .collect()
}

/// Every effect file in the repository: the vfx_lab samples, the regia_vfx test fixtures and the
/// agent fuzz seed.
fn repo_effects() -> Vec<PathBuf> {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    for dir in [
        "samples/vfx_lab/effects",
        "crates/regia_vfx/tests/fixtures",
        "crates/regia_agent/src/fuzz_targets/fixtures",
    ] {
        let Ok(entries) = std::fs::read_dir(base.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.to_string_lossy().ends_with(".vfx.ron") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn the_hand_fixture_lists_exactly_its_modules() {
    let (mut world, _) = project(HAND);
    let state = call(&mut world, "editor.vfx_open", json!({ "path": REL })).expect("open");
    assert_eq!(state["name"], "fixture");
    assert_eq!(
        listed(&state, 0),
        [
            "spawn",
            "init",
            "init_color",
            "gravity",
            "drag",
            "size_over_life",
            "color_over_life",
            "render"
        ]
    );
    assert_eq!(
        listed(&state, 1),
        [
            "spawn",
            "init",
            "gravity",
            "alpha_over_life",
            "kill_below",
            "render"
        ]
    );
    assert_eq!(
        listed(&state, 2),
        [
            "spawn",
            "init",
            "curl",
            "wind",
            "render",
            "flipbook",
            "soft_particles"
        ]
    );
    for e in state["emitters"].as_array().expect("emitters") {
        for m in e["modules"].as_array().expect("modules") {
            assert_eq!(m["enabled"], true);
        }
    }
}

#[test]
fn every_repository_effect_lists_exactly_its_modules() {
    let files = repo_effects();
    assert!(
        files.len() >= 10,
        "found the sample and fixture effects: {files:?}"
    );
    for file in files {
        let text = read(&file);
        let def = VfxSystemDef::from_ron(&text).expect("loads");
        let (mut world, _) = project(&text);
        let state = call(&mut world, "editor.vfx_open", json!({ "path": REL })).expect("open");
        let emitters = state["emitters"].as_array().expect("emitters");
        assert_eq!(emitters.len(), def.emitters.len(), "{}", file.display());
        for (i, e) in def.emitters.iter().enumerate() {
            assert_eq!(emitters[i]["name"], json!(e.name));
            let want: Vec<&str> = present_kinds(e).iter().map(|k| k.name()).collect();
            assert_eq!(listed(&state, i), want, "{} emitter {}", file.display(), e.name);
            // Independent spot checks against the file's fields.
            let has = |k: &str| want.contains(&k);
            assert_eq!(has("vortex"), e.update.vortex.is_some());
            assert_eq!(has("light"), e.render.light.is_some());
            assert_eq!(has("events"), !e.update.events.is_empty());
            assert_eq!(has("kill_volume"), e.update.kill_volume.is_some());
            assert_eq!(has("size_over_life"), !e.update.size_over_life.is_empty());
        }
    }
}

#[test]
fn add_remove_reorder_then_save_simulates_like_the_same_edit_by_hand() {
    let (mut world, file) = project(HAND);
    call(&mut world, "editor.vfx_open", json!({ "path": REL })).expect("open");
    call(
        &mut world,
        "editor.vfx_edit",
        json!({ "op": "add_module", "emitter": "fountain", "module": "vortex" }),
    )
    .expect("add");
    call(
        &mut world,
        "editor.vfx_edit",
        json!({ "op": "remove_module", "emitter": "burst", "module": "gravity" }),
    )
    .expect("remove");
    call(
        &mut world,
        "editor.vfx_edit",
        json!({ "op": "move_emitter", "emitter": "smoke", "to": 0 }),
    )
    .expect("move");
    let saved = call(&mut world, "editor.vfx_edit", json!({ "op": "save" })).expect("save");
    assert!(saved["written"].as_u64().expect("written") > 0);
    let from_editor = VfxSystemDef::from_ron(&read(&file)).expect("the saved file loads");

    // The same edit by hand on the parsed file.
    let mut by_hand = VfxSystemDef::from_ron(HAND).expect("hand");
    by_hand.emitters[0].update.vortex = Some(VortexDef {
        axis: (0.0, 1.0, 0.0),
        strength: 2.0,
        radial: 0.0,
    });
    by_hand.emitters[1].update.gravity = (0.0, 0.0, 0.0);
    let smoke = by_hand.emitters.remove(2);
    by_hand.emitters.insert(0, smoke);
    let by_hand = VfxSystemDef::from_ron(&by_hand.to_ron().expect("ser")).expect("reload");

    assert_eq!(from_editor, by_hand);
    let a = sim_hash(&from_editor, 7, 120);
    let b = sim_hash(&by_hand, 7, 120);
    assert!(a.particles > 0);
    assert_eq!(a, b, "same particle count and positions hash");
    assert_ne!(a, sim_hash(&VfxSystemDef::from_ron(HAND).expect("hand"), 7, 120));
}

#[test]
fn curve_and_gradient_key_edits_round_trip_through_the_file() {
    let (mut world, file) = project(HAND);
    call(&mut world, "editor.vfx_open", json!({ "path": REL })).expect("open");
    let edits = [
        json!({ "op": "curve_add_key", "emitter": "fountain", "curve": "size_over_life", "t": 0.5, "v": 3.0 }),
        json!({ "op": "curve_move_key", "emitter": "fountain", "curve": "size_over_life", "index": 2, "t": 0.9, "v": 2.5 }),
        json!({ "op": "curve_delete_key", "emitter": "burst", "curve": "alpha_over_life", "index": 0 }),
        json!({ "op": "gradient_add_key", "emitter": "fountain", "gradient": "color_over_life", "t": 0.25, "color": "#00ff00" }),
        json!({ "op": "gradient_move_key", "emitter": "fountain", "gradient": "color_over_life", "index": 0, "t": 0.1, "color": "#102030" }),
    ];
    for e in edits {
        call(&mut world, "editor.vfx_edit", e).expect("edit");
    }
    call(&mut world, "editor.vfx_edit", json!({ "op": "save" })).expect("save");
    let def = VfxSystemDef::from_ron(&read(&file)).expect("loads");
    let f = &def.emitters[0];
    assert_eq!(
        f.update.size_over_life,
        [
            FloatKey { t: 0.0, v: 1.0 },
            FloatKey { t: 0.5, v: 3.0 },
            FloatKey { t: 0.9, v: 2.5 }
        ]
    );
    assert_eq!(
        def.emitters[1].update.alpha_over_life,
        [FloatKey { t: 1.0, v: 0.0 }]
    );
    let c = |t: f32, s: &str| ColorKey { t, color: s.into() };
    assert_eq!(
        f.update.color_over_life,
        [c(0.1, "#102030"), c(0.25, "#00ff00"), c(1.0, "#ff4000")]
    );
    // The saved text is the canonical form: it round-trips byte-identically.
    assert_eq!(def.to_ron().expect("ser"), read(&file));
}

#[test]
fn undo_restores_the_file_bytes() {
    let (mut world, file) = project(HAND);
    call(&mut world, "editor.vfx_open", json!({ "path": REL })).expect("open");
    // An unedited save leaves the file alone.
    let s = call(&mut world, "editor.vfx_edit", json!({ "op": "save" })).expect("save");
    assert_eq!(s["written"], 0);
    assert_eq!(read(&file), HAND);
    call(
        &mut world,
        "editor.vfx_edit",
        json!({ "op": "set_param", "emitter": "fountain", "module": "gravity", "param": "y", "value": -2.0 }),
    )
    .expect("edit");
    call(
        &mut world,
        "editor.vfx_edit",
        json!({ "op": "disable_module", "emitter": "smoke", "module": "curl" }),
    )
    .expect("disable");
    call(&mut world, "editor.vfx_edit", json!({ "op": "save" })).expect("save");
    assert_ne!(read(&file), HAND, "the edit reached the file");
    assert_eq!(undo_depth(&world), 2);
    assert!(undo(&mut world));
    assert!(undo(&mut world));
    let s = call(&mut world, "editor.vfx_edit", json!({ "op": "save" })).expect("save");
    assert!(s["written"].as_u64().expect("n") > 0);
    assert_eq!(
        read(&file),
        HAND,
        "undo back to the opened state saves the opened bytes"
    );
    // Redo comes back to the saved edit.
    assert!(redo(&mut world));
    assert!((def_of(&world).emitters[0].update.gravity.1 + 2.0).abs() < 1e-6);
}

#[test]
fn undo_and_redo_after_opening_another_effect_leave_it_and_both_files_alone() {
    let (mut world, file) = project(HAND);
    let other = file.parent().expect("parent").join("other.vfx.ron");
    std::fs::write(&other, HAND).expect("write");
    call(&mut world, "editor.vfx_open", json!({ "path": REL })).expect("open");
    call(
        &mut world,
        "editor.vfx_edit",
        json!({ "op": "set_param", "emitter": "fountain", "module": "gravity", "param": "y", "value": -2.0 }),
    )
    .expect("edit");
    call(
        &mut world,
        "editor.vfx_open",
        json!({ "path": "vfx/other.vfx.ron" }),
    )
    .expect("open other");
    let opened = def_of(&world).to_ron().expect("ser");
    // The undo step belongs to the first effect: neither undo nor redo may write it into the second.
    let _ = undo(&mut world);
    assert_eq!(
        def_of(&world).to_ron().expect("ser"),
        opened,
        "undo left the second effect alone"
    );
    let _ = redo(&mut world);
    assert_eq!(
        def_of(&world).to_ron().expect("ser"),
        opened,
        "redo left the second effect alone"
    );
    let path = world
        .get_resource::<VfxEditorPanel>()
        .and_then(VfxEditorPanel::doc)
        .map(|d| d.path.clone());
    assert_eq!(path.as_deref(), Some("vfx/other.vfx.ron"));
    assert_eq!(read(&file), HAND, "the first file was never saved");
    assert_eq!(read(&other), HAND, "the second file is untouched");
}

#[test]
fn a_refused_edit_leaves_the_document_the_stack_and_the_file_untouched() {
    let (mut world, file) = project(HAND);
    call(&mut world, "editor.vfx_open", json!({ "path": REL })).expect("open");
    call(
        &mut world,
        "editor.vfx_edit",
        json!({ "op": "set_param", "emitter": 0, "module": "drag", "param": "drag", "value": 1.5 }),
    )
    .expect("a good edit first");
    let before = def_of(&world);
    let depth = undo_depth(&world);
    let cases = [
        (
            json!({ "op": "set_param", "emitter": "fountain", "module": "init", "param": "size_min", "value": -1.0 }),
            "vfx_editor_bad_value",
        ),
        (
            json!({ "op": "set_param", "emitter": "smoke", "module": "wind", "param": "response", "value": 2.0 }),
            "vfx_editor_bad_value",
        ),
        (
            json!({ "op": "add_module", "emitter": "fountain", "module": "sparkles" }),
            "vfx_editor_unknown_module",
        ),
        (
            json!({ "op": "set_param", "emitter": "fountain", "module": "spawn", "param": "lifetime_min", "value": 9.0 }),
            "vfx_editor_invalid",
        ),
        (
            json!({ "op": "set_param", "emitter": "fountain", "module": "gravity", "param": "w", "value": 1.0 }),
            "vfx_editor_unknown_param",
        ),
        (
            json!({ "op": "curve_add_key", "emitter": "fountain", "curve": "size_over_life", "t": 1.5, "v": 1.0 }),
            "vfx_editor_bad_value",
        ),
        (
            json!({ "op": "gradient_add_key", "emitter": "fountain", "gradient": "init_color", "t": 0.5, "color": "orange" }),
            "vfx_editor_bad_value",
        ),
        (
            json!({ "op": "remove_module", "emitter": "fountain", "module": "spawn" }),
            "vfx_editor_required_module",
        ),
        (
            json!({ "op": "add_module", "emitter": "fountain", "module": "gravity" }),
            "vfx_editor_already_present",
        ),
        (
            json!({ "op": "add_module", "emitter": "nobody", "module": "drag" }),
            "vfx_editor_unknown_emitter",
        ),
        (
            json!({ "op": "add_module", "emitter": "fountain", "module": "events" }),
            "vfx_editor_not_addable",
        ),
        (
            json!({ "op": "curve_delete_key", "emitter": "fountain", "curve": "size_over_life", "index": 9 }),
            "vfx_editor_bad_index",
        ),
        (
            json!({ "op": "add_emitter", "name": "burst" }),
            "vfx_editor_invalid",
        ),
    ];
    for (params, code) in cases {
        let e = call(&mut world, "editor.vfx_edit", params.clone()).expect_err("refused");
        assert_eq!(reason(&e), code, "{params}");
        assert!(ERROR_CODES.contains(&code));
        assert_eq!(def_of(&world), before, "{params}: document untouched");
        assert_eq!(undo_depth(&world), depth, "{params}: no undo step");
        assert_eq!(read(&file), HAND, "{params}: file untouched");
    }
}

#[test]
fn disabling_a_module_changes_the_simulation_and_enabling_restores_it() {
    let (mut world, _) = project(HAND);
    call(&mut world, "editor.vfx_open", json!({ "path": REL })).expect("open");
    let hash = |world: &mut World| {
        call(world, "editor.vfx_state", json!({ "sim_steps": 90, "seed": 3 })).expect("state")["sim"].clone()
    };
    let original = hash(&mut world);
    assert!(original["particles"].as_u64().expect("n") > 0);
    for module in ["gravity", "drag", "size_over_life"] {
        let s = call(
            &mut world,
            "editor.vfx_edit",
            json!({ "op": "disable_module", "emitter": "fountain", "module": module }),
        )
        .expect("disable");
        let m = s["emitters"][0]["modules"]
            .as_array()
            .expect("modules")
            .iter()
            .find(|m| m["kind"] == module)
            .cloned()
            .expect("still listed");
        assert_eq!(m["enabled"], false, "{module} listed as disabled");
        if module != "size_over_life" {
            // Size does not move particles: positions only change for forces.
            assert_ne!(hash(&mut world), original, "{module} off changes the result");
        }
        let e = call(
            &mut world,
            "editor.vfx_edit",
            json!({ "op": "set_param", "emitter": "fountain", "module": module, "param": "y", "value": 1.0 }),
        )
        .expect_err("a disabled module is not edited");
        assert_eq!(reason(&e), "vfx_editor_module_disabled");
        call(
            &mut world,
            "editor.vfx_edit",
            json!({ "op": "enable_module", "emitter": "fountain", "module": module }),
        )
        .expect("enable");
        assert_eq!(hash(&mut world), original, "{module} back on restores the hash");
    }
    // Disabled modules are not written; the editor keeps them.
    call(
        &mut world,
        "editor.vfx_edit",
        json!({ "op": "disable_module", "emitter": "burst", "module": "gravity" }),
    )
    .expect("disable");
    let s = call(&mut world, "editor.vfx_edit", json!({ "op": "save" })).expect("save");
    assert_eq!(s["disabled_not_saved"], 1);
}

/// `docs/specs/19` § 7 follow-up (ADR-7340): the VFX autosave keeps the disabled-module stash in
/// the editor-only autosave entry (never cooked or packaged); recovery restores the module still
/// disabled with its values, dirty until a save.
#[test]
fn autosave_keeps_the_disabled_module_stash() {
    let (mut world, file) = project(HAND);
    crate::doc_autosave::install(&mut world);
    open_effect(&mut world, REL).expect("open");
    edit_effect(&mut world, &VfxChange::SetEnabled(0, ModuleKind::Drag, false)).expect("disable");
    let kept = {
        let doc = world
            .get_resource::<VfxEditorPanel>()
            .and_then(VfxEditorPanel::doc)
            .expect("doc");
        assert!(doc.dirty(), "disabling a module edits the effect");
        assert!(doc.is_disabled("fountain", ModuleKind::Drag));
        doc.state.disabled.clone()
    };
    assert_eq!(
        kept,
        vec![Disabled {
            emitter: "fountain".into(),
            data: ModuleData::Drag(0.5),
        }],
        "the fixture's drag is stashed"
    );
    // The registry's snapshot carries the stash (in the editor-only envelope).
    let snapshots = crate::doc_autosave::take_snapshots(&world);
    let snap = snapshots
        .iter()
        .find(|s| s.key.kind == crate::vfx_editor::autosave::KIND)
        .expect("a vfx autosave snapshot");

    // A fresh editor recovers it: the module is still disabled with its values.
    let (mut fresh, _) = project(HAND);
    crate::doc_autosave::install(&mut fresh);
    let provider = fresh
        .resource::<crate::doc_autosave::DocAutosaveRegistry>()
        .provider(crate::vfx_editor::autosave::KIND)
        .expect("the vfx provider is registered");
    (provider.restore)(&mut fresh, &snap.key, &snap.bytes).expect("restore");
    let doc = fresh
        .get_resource::<VfxEditorPanel>()
        .and_then(VfxEditorPanel::doc)
        .expect("the effect is open");
    assert!(
        doc.is_disabled("fountain", ModuleKind::Drag),
        "the module is still disabled after recovery"
    );
    assert_eq!(doc.state.disabled, kept, "its values are kept");
    assert!(doc.dirty(), "dirty until a save");
    let root = file.parent().and_then(Path::parent).expect("root").to_path_buf();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn cost_rows_exist_for_every_emitter() {
    let def = VfxSystemDef::from_ron(HAND).expect("hand");
    let rows = emitter_costs(&def, 1, 1.0, None);
    assert_eq!(rows.len(), def.emitters.len());
    for (r, e) in rows.iter().zip(&def.emitters) {
        assert_eq!(r.name, e.name);
        assert!(r.gpu_ms.is_none(), "no GPU time without a timed frame");
    }
    let fountain = &rows[0];
    assert!(fountain.alive > 0);
    let rate = fountain.spawned_per_sec.expect("scheduled emitter");
    assert!((rate - 60.0).abs() <= 2.0, "60/s spawns about 60/s, got {rate}");
    assert_eq!(fountain.draw_list, "additive");
    assert_eq!(fountain.draw_calls, 1);
    assert_eq!(rows[2].draw_list, "alpha");
    // The burst of 40 fires at t = 0: inside the one-second window that ends at t = 1, outside the
    // one that ends at t = 2.
    assert_eq!(rows[1].spawned_per_sec, Some(40.0));
    let later = emitter_costs(&def, 1, 2.0, None);
    assert_eq!(later[1].spawned_per_sec, Some(0.0));
    assert!((later[0].spawned_per_sec.expect("rate") - 60.0).abs() <= 2.0);
    // GPU time splits by live particles and sums to the total.
    let timed = emitter_costs(&def, 1, 1.0, Some(2.0));
    let sum: f32 = timed.iter().filter_map(|r| r.gpu_ms).sum();
    assert!((sum - 2.0).abs() < 1e-4);
    // Over the agent.
    let (mut world, _) = project(HAND);
    call(&mut world, "editor.vfx_open", json!({ "path": REL })).expect("open");
    let s = call(&mut world, "editor.vfx_state", json!({ "cost_time": 1.0 })).expect("state");
    let costs = s["costs"].as_array().expect("costs");
    assert_eq!(costs.len(), 3);
    assert_eq!(costs[0]["gpu_source"], "cpu_counts");
}

#[test]
fn modules_round_trip_through_the_stash_for_every_kind() {
    let def = VfxSystemDef::from_ron(HAND).expect("hand");
    for kind in modules::palette() {
        let mut e = def.emitters[0].clone();
        if !modules::present(&e, kind) {
            modules::put(&mut e, modules::ModuleData::default_for(kind).expect("default"));
        }
        assert!(modules::present(&e, kind), "{kind:?} present after put");
        let with = e.clone();
        let data = modules::take(&mut e, kind).expect("take");
        assert!(!modules::present(&e, kind), "{kind:?} absent after take");
        assert_eq!(data.kind(), kind);
        modules::put(&mut e, data);
        assert_eq!(e, with, "{kind:?} restored exactly");
    }
    assert!(ModuleKind::from_name("vortex").is_some());
    assert!(ModuleKind::from_name("sparkles").is_none());
}

#[test]
fn every_palette_module_adds_a_valid_effect_and_emitters_add_and_remove() {
    let (mut world, _) = project(HAND);
    open_effect(&mut world, REL).expect("open");
    // The smoke emitter is alpha with a flipbook: every addable kind it lacks must validate.
    for kind in modules::palette() {
        let def = def_of(&world);
        if modules::present(&def.emitters[2], kind) {
            continue;
        }
        edit_effect(&mut world, &VfxChange::AddModule(2, kind)).unwrap_or_else(|e| panic!("{kind:?}: {e:?}"));
    }
    edit_effect(&mut world, &VfxChange::AddEmitter("sparks".into())).expect("add emitter");
    assert_eq!(def_of(&world).emitters.len(), 4);
    edit_effect(&mut world, &VfxChange::RemoveEmitter(3)).expect("remove emitter");
    assert_eq!(def_of(&world).emitters.len(), 3);
}

#[test]
fn every_error_code_is_documented_in_the_book() {
    let book = include_str!("../../../../book/agents/vfx.md");
    for code in ERROR_CODES {
        assert!(book.contains(code), "{code} missing from book/agents/vfx.md");
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{code}"
        );
    }
    for m in methods() {
        assert!(book.contains(m.0.name), "{} missing from the book page", m.0.name);
    }
    for op in super::agent::EDIT_OPS {
        assert!(book.contains(op), "op {op} missing from the book page");
    }
}

#[test]
fn errors_without_an_open_effect_or_project() {
    let mut world = World::new();
    let e = call(&mut world, "editor.vfx_open", json!({ "path": REL })).expect_err("no project");
    assert_eq!(reason(&e), "vfx_editor_no_project");
    let s = call(&mut world, "editor.vfx_state", json!({})).expect("state");
    assert_eq!(s["open"], false);
    let e = call(
        &mut world,
        "editor.vfx_edit",
        json!({ "op": "add_module", "emitter": 0, "module": "drag" }),
    )
    .expect_err("nothing open");
    assert_eq!(reason(&e), "vfx_editor_no_effect");
    let (mut world, _) = project(HAND);
    let e = call(
        &mut world,
        "editor.vfx_open",
        json!({ "path": "../escape.vfx.ron" }),
    )
    .expect_err("bad path");
    assert_eq!(reason(&e), "vfx_editor_bad_path");
    let e = call(
        &mut world,
        "editor.vfx_open",
        json!({ "path": "vfx/missing.vfx.ron" }),
    )
    .expect_err("io");
    assert_eq!(reason(&e), "vfx_editor_io_failed");
    let (mut world, _) = project("(version: 7, name: \"x\", emitters: [])");
    let e = call(&mut world, "editor.vfx_open", json!({ "path": REL })).expect_err("invalid file");
    assert_eq!(reason(&e), "vfx_editor_parse_failed");
}

#[test]
fn the_panel_draws_headless_with_modules_curves_and_a_disabled_module() {
    let (mut world, _) = project(HAND);
    open_effect(&mut world, REL).expect("open");
    let shapes = draw_frames(&mut world, 2, panel::vfx_editor_panel_no_gpu);
    assert!(!shapes.is_empty());
    assert!(has_color(&shapes, panel::CURVE_COLOR), "the size curve is drawn");
    assert!(has_color(&shapes, panel::KEY_COLOR), "curve keys are drawn");
    assert!(!has_color(&shapes, panel::DISABLED_COLOR));
    edit_effect(&mut world, &VfxChange::SetEnabled(0, ModuleKind::Drag, false)).expect("disable");
    let shapes = draw_frames(&mut world, 2, panel::vfx_editor_panel_no_gpu);
    assert!(
        has_color(&shapes, panel::DISABLED_COLOR),
        "the disabled module is greyed"
    );
}

/// A headless GPU device, or `None` (the preview tests then say so and pass: they run on the build
/// host's lavapipe).
fn gpu() -> Option<regia_render::RenderDevice> {
    match crate::render::headless_device() {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("skipping the VFX preview test: no headless GPU ({e})");
            None
        }
    }
}

#[test]
fn the_preview_renders_a_frame_with_coverage_and_a_png() {
    let Some(device) = gpu() else {
        return;
    };
    let (mut world, _) = project(HAND);
    world.insert_resource(device);
    call(&mut world, "editor.vfx_open", json!({ "path": REL })).expect("open");
    let p = call(
        &mut world,
        "editor.vfx_preview",
        json!({ "time": 0.5, "seed": 4, "width": 160, "height": 96, "png": true }),
    )
    .expect("preview");
    assert_eq!(p["steps"], 30);
    assert!(p["coverage"].as_f64().expect("coverage") > 0.0, "{p}");
    assert!(p["gpu_alive"].as_u64().expect("alive") > 0, "{p}");
    assert_eq!(p["costs"].as_array().expect("costs").len(), 3);
    let png = PathBuf::from(p["png"].as_str().expect("png path"));
    let bytes = std::fs::read(&png).expect("png written");
    assert_eq!(&bytes[1..4], b"PNG");
    let _ = std::fs::remove_file(&png);
    // A later time steps the same session on; an earlier one restarts and reaches the same step count.
    let later = call(
        &mut world,
        "editor.vfx_preview",
        json!({ "time": 1.0, "seed": 4, "width": 160, "height": 96 }),
    )
    .expect("later");
    assert_eq!(later["steps"], 60);
    let again = call(
        &mut world,
        "editor.vfx_preview",
        json!({ "time": 0.5, "seed": 4, "width": 160, "height": 96 }),
    )
    .expect("again");
    assert_eq!(again["steps"], 30);
    // A refused size is a documented code.
    let e = call(&mut world, "editor.vfx_preview", json!({ "width": 4 })).expect_err("too small");
    assert_eq!(reason(&e), "vfx_editor_bad_value");
}

#[test]
fn the_preview_follows_the_ab_toggle() {
    let Some(device) = gpu() else {
        return;
    };
    let (mut world, _) = project(HAND);
    world.insert_resource(device);
    call(&mut world, "editor.vfx_open", json!({ "path": REL })).expect("open");
    let on = call(
        &mut world,
        "editor.vfx_preview",
        json!({ "time": 1.0, "width": 128, "height": 72 }),
    )
    .expect("on");
    let rev_on = on["revision"].as_u64().expect("rev");
    call(
        &mut world,
        "editor.vfx_edit",
        json!({ "op": "disable_module", "emitter": "fountain", "module": "gravity" }),
    )
    .expect("disable");
    let off = call(
        &mut world,
        "editor.vfx_preview",
        json!({ "time": 1.0, "width": 128, "height": 72 }),
    )
    .expect("off");
    assert_ne!(
        off["revision"].as_u64().expect("rev"),
        rev_on,
        "the edit restarted the preview"
    );
    assert!(off["coverage"].as_f64().expect("c") > 0.0);
}

/// FU-5: a curve's key list focuses a key (Tab, Down), moves it in time (Left, one undo entry)
/// and in value (Alt+Up), and deletes another key (Delete) with keys only; the effect equals the
/// one the mouse's key edits (the agent's `curve_move_key` / `curve_delete_key`) build.
#[test]
fn a_curve_key_list_edits_by_keyboard_what_the_mouse_edits() {
    use crate::canvas_keys::test_input::{focused_name, frame, key, labels_of, tab_to};
    use egui::{Key, Modifiers};
    const SIZE: [f32; 2] = [1200.0, 2400.0];
    let (mut world, _) = project(HAND);
    open_effect(&mut world, REL).expect("open");
    let ctx = egui::Context::default();
    let draw = |world: &mut World, events: Vec<egui::Event>| {
        frame(&ctx, SIZE, events, |ui| panel::vfx_editor_panel_no_gpu(ui, world))
    };
    let first = draw(&mut world, vec![]);
    assert!(
        labels_of(&first, egui::accesskit::Role::Canvas)
            .contains(&"size_over_life curve canvas, 2 keys".to_string()),
        "{:?}",
        labels_of(&first, egui::accesskit::Role::Canvas)
    );
    tab_to(
        &ctx,
        SIZE,
        "size_over_life key 1 of 2: time 0.00, value 1.00",
        false,
        600,
        |ui| panel::vfx_editor_panel_no_gpu(ui, &mut world),
    );
    draw(&mut world, vec![key(Key::ArrowDown, Modifiers::NONE)]);
    let down = draw(&mut world, vec![]);
    assert_eq!(
        focused_name(&down).as_deref(),
        Some("size_over_life key 2 of 2: time 1.00, value 2.00")
    );
    let depth = undo_depth(&world);
    // Key 2 sits at t 1.0: Right clamps there; Left moves it in.
    draw(&mut world, vec![key(Key::ArrowLeft, Modifiers::NONE)]);
    assert_eq!(undo_depth(&world), depth + 1, "one entry per press");
    draw(&mut world, vec![key(Key::ArrowUp, Modifiers::ALT)]);
    draw(&mut world, vec![key(Key::ArrowUp, Modifiers::NONE)]);
    draw(&mut world, vec![]);
    draw(&mut world, vec![key(Key::Delete, Modifiers::NONE)]);
    // The value press merged into the time press's entry: edits of one key coalesce as a drag's
    // do (`VfxEdit::merge`, the mouse's own path); the delete is its own entry.
    assert_eq!(undo_depth(&world), depth + 2);
    // The same key edits as the mouse makes (drag, drag, right-click), through the agent's path.
    let (mut mouse, _) = project(HAND);
    open_effect(&mut mouse, REL).expect("open");
    let keys = CurveRef::SizeOverLife.keys(&def_of(&mouse).emitters[0]).to_vec();
    assert_eq!(keys.len(), 2, "the fixture's size curve");
    let (t, v) = panel::nudged_key(&keys, 1, -1, 0, false).expect("key 2");
    let moved = [keys[0], regia_vfx::format::FloatKey { t, v }];
    let (t2, v2) = panel::nudged_key(&moved, 1, 0, -1, false).expect("key 2");
    for change in [
        VfxChange::Key(0, KeyEdit::MoveKey(CurveRef::SizeOverLife, 1, t, v)),
        VfxChange::Key(0, KeyEdit::MoveKey(CurveRef::SizeOverLife, 1, t2, v2)),
        VfxChange::Key(0, KeyEdit::DeleteKey(CurveRef::SizeOverLife, 0)),
    ] {
        edit_effect(&mut mouse, &change).expect("mouse edit");
    }
    assert_eq!(def_of(&world).to_ron().ok(), def_of(&mouse).to_ron().ok());
    assert_eq!(CurveRef::SizeOverLife.keys(&def_of(&world).emitters[0]).len(), 1);
}

#[test]
fn w12_state_reports_emitter_timeline_and_computed_bounds() {
    let (mut world, file) = project(HAND);
    open_effect(&mut world, REL).expect("open");
    let state = call(
        &mut world,
        "editor.vfx_state",
        json!({"bounds_time": 0.5, "seed": 7}),
    )
    .expect("state");
    assert_eq!(state["timeline"][1]["burst_count"], 40);
    assert_eq!(state["timeline"][1]["start"], 0.0);
    assert_eq!(state["bounds"]["mode"], "computed");
    assert_eq!(state["bounds"]["emitters"].as_array().expect("emitters").len(), 3);
    assert!(state["bounds"]["system"]["min"].is_array(), "{state}");
    assert_eq!(read(&file), HAND, "readouts never change the asset");
}

#[test]
fn w12_overview_enable_solo_and_scrub_are_view_only() {
    let (mut world, file) = project(HAND);
    open_effect(&mut world, REL).expect("open");
    let depth = undo_depth(&world);
    let out = call(
        &mut world,
        "editor.vfx_edit",
        json!({"op": "preview_emitter", "emitter": "burst", "enabled": false, "solo": true}),
    )
    .expect("preview emitter");
    assert_eq!(out["overview"][1]["enabled"], false);
    assert_eq!(out["overview"][1]["solo"], true);
    let out = call(
        &mut world,
        "editor.vfx_edit",
        json!({"op": "scrub", "time": 0.75}),
    )
    .expect("scrub");
    assert_eq!(out["preview_time"], 0.75);
    assert_eq!(undo_depth(&world), depth);
    assert_eq!(read(&file), HAND);
}

#[test]
fn w12_preview_isolation_changes_render_without_changing_asset() {
    let device = crate::render::headless_device().expect("lavapipe for VFX acceptance");
    let (mut world, file) = project(HAND);
    world.insert_resource(device);
    open_effect(&mut world, REL).expect("open");
    let before = read(&file);
    let on = call(
        &mut world,
        "editor.vfx_preview",
        json!({"time":0.5,"seed":4,"width":128,"height":72,"png":true}),
    )
    .expect("on");
    let png_on = PathBuf::from(on["png"].as_str().expect("PNG path"));
    let pixels_on = std::fs::read(&png_on).expect("on image");
    for name in ["fountain", "burst", "smoke"] {
        call(
            &mut world,
            "editor.vfx_edit",
            json!({"op":"preview_emitter","emitter":name,"enabled":false}),
        )
        .expect("isolate");
    }
    let off = call(
        &mut world,
        "editor.vfx_preview",
        json!({"time":0.5,"seed":4,"width":128,"height":72,"png":true}),
    )
    .expect("off");
    let png_off = PathBuf::from(off["png"].as_str().expect("PNG path"));
    let pixels_off = std::fs::read(&png_off).expect("off image");
    assert!(on["gpu_alive"].as_u64().expect("alive") > 0);
    assert_eq!(off["gpu_alive"], 0);
    let on_image = image::load_from_memory(&pixels_on).expect("decode on").to_rgba8();
    let off_image = image::load_from_memory(&pixels_off)
        .expect("decode off")
        .to_rgba8();
    assert_ne!(on_image, off_image, "control-render pixel difference; no golden");
    assert_eq!(read(&file), before);
    let _ = std::fs::remove_file(png_on);
    let _ = std::fs::remove_file(png_off);
}

/// P-225: the fixed bounds through `editor.vfx_edit` (set from measured, set, refuse, clear, undo),
/// reported by `editor.vfx_state`, saved into the `.vfx.ron` and loaded back.
#[test]
fn w13_fixed_bounds_set_from_measured_saved_and_reported() {
    let (mut world, file) = project(HAND);
    open_effect(&mut world, REL).expect("open");
    let e = call(
        &mut world,
        "editor.vfx_edit",
        json!({"op": "fixed_bounds_from_measured", "time": 0.0, "seed": 7}),
    )
    .expect_err("nothing alive at 0 s");
    assert_eq!(reason(&e), "vfx_editor_empty_bounds");
    let out = call(
        &mut world,
        "editor.vfx_edit",
        json!({"op": "fixed_bounds_from_measured", "time": 0.5, "seed": 7}),
    )
    .expect("from measured");
    assert_eq!(out["changed"], true);
    assert!(out["fixed_bounds"]["min"].is_array(), "{out}");
    let state = call(
        &mut world,
        "editor.vfx_state",
        json!({"bounds_time": 0.5, "seed": 7}),
    )
    .expect("state");
    assert_eq!(state["bounds"]["mode"], "fixed");
    for i in 0..3 {
        let at = |k: &str, which: &str| state["bounds"][which][k][i].as_f64().expect("number");
        assert!(at("min", "fixed_world") <= at("min", "system") + 1e-4, "{state}");
        assert!(at("max", "fixed_world") >= at("max", "system") - 1e-4, "{state}");
    }
    // An explicit box; a refused one changes nothing.
    call(
        &mut world,
        "editor.vfx_edit",
        json!({"op": "set_fixed_bounds", "min": [-2.0, 0.0, -2.0], "max": [2.0, 5.0, 2.0]}),
    )
    .expect("set");
    let set = regia_vfx::format::FixedBoundsDef {
        min: [-2.0, 0.0, -2.0],
        max: [2.0, 5.0, 2.0],
    };
    assert_eq!(def_of(&world).system.bounds, Some(set));
    let depth = undo_depth(&world);
    let e = call(
        &mut world,
        "editor.vfx_edit",
        json!({"op": "set_fixed_bounds", "min": [3.0, 0.0, 0.0], "max": [1.0, 1.0, 1.0]}),
    )
    .expect_err("min > max");
    assert_eq!(reason(&e), "vfx_editor_bad_value");
    assert!(
        call(
            &mut world,
            "editor.vfx_edit",
            json!({"op": "set_fixed_bounds", "min": [0.0, 0.0]})
        )
        .is_err(),
        "a short corner is refused"
    );
    assert_eq!(def_of(&world).system.bounds, Some(set));
    assert_eq!(undo_depth(&world), depth);
    // Saved into the file, loaded back by the runtime loader.
    call(&mut world, "editor.vfx_edit", json!({"op": "save"})).expect("save");
    let saved = VfxSystemDef::from_ron(&read(&file)).expect("saved file loads");
    assert_eq!(saved.system.bounds, Some(set));
    // Clear, then undo brings the box back.
    call(&mut world, "editor.vfx_edit", json!({"op": "clear_fixed_bounds"})).expect("clear");
    assert_eq!(def_of(&world).system.bounds, None);
    call(&mut world, "editor.vfx_edit", json!({"op": "undo"})).expect("undo");
    assert_eq!(def_of(&world).system.bounds, Some(set));
}

/// P-225 (2): `editor.vfx_preview { bounds: true }` reduces the GPU pool to per-emitter bounds
/// (lavapipe on the build host); without the flag nothing is read.
#[test]
fn w13_preview_reports_gpu_bounds_on_request() {
    let Some(device) = gpu() else {
        return;
    };
    let (mut world, _) = project(HAND);
    world.insert_resource(device);
    call(&mut world, "editor.vfx_open", json!({ "path": REL })).expect("open");
    let plain = call(
        &mut world,
        "editor.vfx_preview",
        json!({ "time": 0.5, "seed": 4, "width": 96, "height": 64 }),
    )
    .expect("preview");
    assert!(plain.get("gpu_bounds").is_none(), "opt-in only");
    let p = call(
        &mut world,
        "editor.vfx_preview",
        json!({ "time": 0.75, "seed": 4, "width": 96, "height": 64, "bounds": true }),
    )
    .expect("preview with bounds");
    let b = &p["gpu_bounds"];
    assert_eq!(b["source"], "gpu_reduction");
    assert_eq!(b["emitters"].as_array().expect("emitters").len(), 3);
    for i in 0..3 {
        let lo = b["system"]["min"][i].as_f64().expect("min");
        let hi = b["system"]["max"][i].as_f64().expect("max");
        assert!(lo.is_finite() && hi.is_finite() && lo <= hi, "{b}");
    }
}
