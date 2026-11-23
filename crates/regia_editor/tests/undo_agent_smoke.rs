// SPDX-License-Identifier: MPL-2.0
//! Agent-driven editor smoke test for undo/redo (`book/agents/undo-redo.md`).
//!
//! Every edit command the editor has is made through the agent protocol, through the real
//! dispatcher (permissions, rate limit, audit and provenance), fed from an in-process inbox
//! instead of the socket so each call is answered within one deterministic frame (no
//! wall-clock waits). After every step the canonical scene is recorded; undoing step by step must
//! give back each recorded scene exactly, redoing must give them back again, every restored entity
//! must still belong to the open level (so a save keeps it), ids from before an undo must keep
//! working, and dirty tracking must follow the saved state.

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::level_io::{self, OpenLevel};
use regia_editor::selection::Selection;
use regia_scene::prelude::{LevelId, snapshot_scene};
use regia_scene::spawn::SnapshotFilter;
use serde_json::{Value, json};

/// A reflected component the smoke test adds, edits and removes (distinct name: short-name
/// lookups must not be ambiguous).
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct UndoSmokeTag {
    value: f32,
}

/// An agent talking to the edit world through the dispatcher.
struct Agent {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
    level: LevelId,
}

impl Agent {
    /// The editor binary's edit world with the agent plugin; the socket inbox is swapped for an
    /// in-process one.
    fn new(project: &std::path::Path) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.to_path_buf()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.init_resource::<UndoStack>()
            .init_resource::<Selection>()
            .init_resource::<regia_editor::viewport::EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<regia_editor::viewport::ViewportSettings>();
        app.register_type::<UndoSmokeTag>();
        let open = level_io::new_level(app.world_mut());
        let level = open.id;
        app.world_mut().insert_resource(open);
        regia_editor::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded::<Inbound>();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        // The default profile allows every edit; lift the per-second budget so the test never
        // depends on how fast it runs.
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        app.world_mut().insert_resource(permissions);
        Self {
            app,
            inbox,
            next_id: 0,
            level,
        }
    }

    fn try_call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next_id += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(self.next_id)),
            method: method.into(),
            params,
        };
        self.inbox.send(Inbound { request, reply }).expect("inbox open");
        self.app.update();
        let response = answer
            .try_recv()
            .expect("the dispatcher answers within the frame");
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(value), None) => Ok(value),
            (None, None) => panic!("{method}: empty response"),
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        match self.try_call(method, params.clone()) {
            Ok(value) => value,
            Err(err) => panic!("{method} {params}: {err:?}"),
        }
    }

    fn entity(&mut self, method: &str, params: Value) -> u64 {
        let out = self.call(method, params);
        out["entity"]
            .as_u64()
            .unwrap_or_else(|| panic!("{method}: no entity in {out}"))
    }

    /// The whole scene as canonical RON, entities sorted by guid (a respawn moves an entity to
    /// a new table row, which reorders the raw snapshot without changing the scene).
    fn scene(&mut self) -> String {
        let filter = SnapshotFilter {
            level: None,
            resources: vec!["regia_scene::level::LevelSettings".to_string()],
        };
        let mut file = snapshot_scene(self.app.world_mut(), &filter).expect("snapshot");
        file.entities.sort_by_key(|entity| entity.guid);
        file.to_ron_string().expect("ron")
    }

    /// (entities with scene identity, of those the ones in the open level). A level save
    /// writes only the second set.
    fn membership(&mut self) -> (usize, usize) {
        fn count(world: &mut World, level: Option<LevelId>) -> usize {
            let filter = SnapshotFilter {
                level,
                resources: Vec::new(),
            };
            snapshot_scene(world, &filter).expect("snapshot").entities.len()
        }
        let level = self.level;
        let world = self.app.world_mut();
        (count(world, None), count(world, Some(level)))
    }

    fn history(&mut self) -> Value {
        self.call("editor.history", json!({}))
    }

    fn depth(&mut self) -> u64 {
        self.history()["undo_depth"].as_u64().expect("depth")
    }
}

/// The error's Regia Engine code (`F0002` invalid params, `F0003` not found).
fn code(err: &RpcError) -> &str {
    err.data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

#[test]
fn every_edit_command_undoes_and_redoes_step_by_step_through_the_agent() {
    let project = regia_test_utils::unique_temp_dir("editor-undo-smoke");
    std::fs::create_dir_all(project.join("assets")).expect("assets dir");
    let mut agent = Agent::new(&project);

    let history = agent.history();
    assert_eq!(history["undo_depth"], 0, "{history}");
    assert_eq!(history["dirty"], false, "a new level starts clean");

    let mut states = vec![agent.scene()];
    let mut labels: Vec<&str> = Vec::new();
    let mut step = |agent: &mut Agent, label: &'static str| {
        states.push(agent.scene());
        labels.push(label);
        let (all, members) = agent.membership();
        assert_eq!(all, members, "after {label}: every entity is in the level");
        assert_eq!(
            agent.depth(),
            labels.len() as u64,
            "after {label}: one entry per call"
        );
    };

    let base = agent.entity("editor.spawn", json!({ "name": "Base" }));
    step(&mut agent, "spawn base");
    let rock = agent.entity(
        "editor.spawn",
        json!({ "name": "Rock", "parent": base, "translation": [1.0, 0.0, 0.0] }),
    );
    step(&mut agent, "spawn rock under base");
    let pebble = agent.entity("editor.spawn", json!({ "name": "Pebble", "parent": rock }));
    step(&mut agent, "spawn pebble under rock");

    // Refused edits change nothing and push nothing.
    let cycle = agent
        .try_call("editor.reparent", json!({ "entity": base, "parent": pebble }))
        .expect_err("a cycle");
    assert_eq!(code(&cycle), "F0002", "{cycle:?}");
    let empty = agent
        .try_call("editor.delete", json!({ "entities": [] }))
        .expect_err("nothing to delete");
    assert_eq!(code(&empty), "F0002");
    let missing = agent
        .try_call(
            "editor.remove_component",
            json!({ "entity": pebble, "component": "UndoSmokeTag" }),
        )
        .expect_err("no tag yet");
    assert_eq!(code(&missing), "F0003", "{missing:?}");
    // editor.reparent is the outliner's move (ADR-866, unified with ADR-540 at integration):
    // the current parent is refused as `reparent_nothing_to_move` and pushes nothing.
    let same = agent
        .try_call("editor.reparent", json!({ "entity": rock, "parent": base }))
        .expect_err("already under that parent");
    assert_eq!(code(&same), "F0002", "{same:?}");
    assert_eq!(agent.depth(), 3, "refusals and no-ops pushed nothing");

    agent.call(
        "editor.set_transform",
        json!({ "entity": rock, "translation": [3.0, 0.0, -2.0] }),
    );
    step(&mut agent, "set_transform rock");
    agent.call(
        "editor.set_property",
        json!({ "entity": rock, "component": "Transform", "path": "translation.y", "value": 2.0 }),
    );
    step(&mut agent, "set_property rock y");
    agent.call(
        "editor.add_component",
        json!({ "entity": pebble, "component": "UndoSmokeTag", "value": { "value": 0.5 } }),
    );
    step(&mut agent, "add tag to pebble");
    let again = agent
        .try_call(
            "editor.add_component",
            json!({ "entity": pebble, "component": "UndoSmokeTag" }),
        )
        .expect_err("already there");
    assert_eq!(code(&again), "F0002");
    for value in [0.75, 0.8] {
        agent.call(
            "editor.set_property",
            json!({ "entity": pebble, "component": "UndoSmokeTag", "path": "value", "value": value }),
        );
        step(&mut agent, "set_property pebble tag");
    }
    let copy = agent.call("editor.duplicate", json!({ "entity": rock }));
    assert_eq!(
        copy["parent"], base,
        "the copy stays under the original's parent: {copy}"
    );
    step(&mut agent, "duplicate rock");
    agent.call("editor.reparent", json!({ "entity": pebble, "parent": null }));
    step(&mut agent, "reparent pebble to root");
    agent.call(
        "editor.remove_component",
        json!({ "entity": pebble, "component": "UndoSmokeTag" }),
    );
    step(&mut agent, "remove tag from pebble");
    agent.call("editor.select", json!({ "entities": [rock], "mode": "set" }));
    agent.call("editor.delete", json!({ "entities": [base] }));
    step(&mut agent, "delete base subtree");
    let selection = agent.call("editor.get_selection", json!({}));
    assert_eq!(
        selection["entities"],
        json!([]),
        "the deleted rock left the selection: {selection}"
    );

    let history = agent.history();
    assert_eq!(history["undo"][0], "Delete 1 entities", "{history}");
    assert_eq!(history["dirty"], true);

    // Undo step by step: each one gives back the scene before that step, exactly.
    // `labels[i]` took the scene from `states[i]` to `states[i + 1]`.
    for (label, before) in labels.iter().zip(&states).rev() {
        let out = agent.call("editor.undo", json!({}));
        assert_eq!(out["undone"], true, "undo of {label}");
        assert!(
            agent.scene() == *before,
            "undo of `{label}` did not restore the scene before it:\n--- expected\n{before}\n--- got\n{}",
            agent.scene()
        );
        let (all, members) = agent.membership();
        assert_eq!(
            all, members,
            "undo of {label}: restored entities are in the level"
        );
    }
    let out = agent.call("editor.undo", json!({}));
    assert_eq!(out["undone"], false, "nothing left");
    assert_eq!(
        agent.history()["dirty"],
        false,
        "back to the opened, never-saved level"
    );

    // Redo step by step: each one gives back the scene after that step.
    for (label, after) in labels.iter().zip(states.iter().skip(1)) {
        let out = agent.call("editor.redo", json!({}));
        assert_eq!(out["redone"], true, "redo of {label}");
        assert!(
            agent.scene() == *after,
            "redo of `{label}` did not give back its scene:\n--- expected\n{after}\n--- got\n{}",
            agent.scene()
        );
        let (all, members) = agent.membership();
        assert_eq!(all, members, "redo of {label}");
    }
    assert_eq!(agent.history()["redo_depth"], 0);

    // The pebble was re-created by the redo of its spawn: the id from the first run still
    // addresses it.
    let moved = agent.call(
        "editor.set_transform",
        json!({ "entity": pebble, "translation": [0.0, 5.0, 0.0] }),
    );
    assert_ne!(
        moved["entity"], pebble,
        "a new entity id behind the old one: {moved}"
    );
    assert!(agent.history()["remapped"].as_u64().unwrap_or(0) > 0);
    agent.call("editor.undo", json!({}));

    // Save, then dirty follows the saved state through undo and redo.
    let path = project.join("assets").join("smoke.level.ron");
    agent.call("editor.save_level", json!({ "path": path.display().to_string() }));
    assert_eq!(agent.history()["dirty"], false, "saved");
    agent.call("editor.undo", json!({}));
    assert_eq!(agent.history()["dirty"], true, "one step below the save");
    agent.call("editor.redo", json!({}));
    assert_eq!(agent.history()["dirty"], false, "redone back to the save");
    let open = agent.app.world().resource::<OpenLevel>().clone();
    assert_eq!(open.path.as_deref(), Some(path.as_path()));
    let saved = std::fs::read_to_string(&path).expect("saved level");
    assert!(saved.contains("Pebble"), "{saved}");

    // Undo the delete and save: the restored subtree is in the level file.
    agent.call("editor.undo", json!({}));
    agent.call("editor.save_level", json!({}));
    let saved = std::fs::read_to_string(&path).expect("saved level");
    assert!(
        saved.contains("Base") && saved.matches("Rock").count() >= 2,
        "the restored base, rock and its copy are saved: {saved}"
    );

    let _ = std::fs::remove_dir_all(&project);
}

/// Audit v033b (Med): `editor.set_transform` cast JSON numbers to `f32` without a finite check,
/// so `1e39` became infinity on the `Transform` (and an undo step and the next save); the same
/// cast fed `editor.set_render_override`. Both refuse it and leave the entity unchanged.
#[test]
fn transform_and_override_calls_refuse_numbers_outside_the_f32_range() {
    let project = regia_test_utils::unique_temp_dir("editor-agent-finite");
    std::fs::create_dir_all(project.join("assets")).expect("assets dir");
    let mut agent = Agent::new(&project);
    let rock = agent.entity("editor.spawn", json!({ "name": "Rock" }));
    let depth = agent.depth();
    for params in [
        json!({ "entity": rock, "translation": [1e39, 0.0, 0.0] }),
        json!({ "entity": rock, "scale": [1.0, -1e39, 1.0] }),
        json!({ "entity": rock, "rotation": [1e39, 0.0, 0.0, 1.0] }),
    ] {
        let err = agent
            .try_call("editor.set_transform", params.clone())
            .expect_err("a number outside the f32 range is refused");
        assert_eq!(code(&err), "F0002", "{params}: {err:?}");
    }
    assert_eq!(agent.depth(), depth, "no undo step was pushed");
    let entity = regia_ecs::ecs::entity::Entity::try_from_bits(rock).expect("bits");
    let transform = *agent
        .app
        .world()
        .get::<regia_scene::prelude::Transform>(entity)
        .expect("transform");
    assert!(
        transform.translation.is_finite() && transform.rotation.is_finite() && transform.scale.is_finite()
    );

    for params in [
        json!({ "entity": rock, "tint": [1e39, 1.0, 1.0, 1.0] }),
        json!({ "entity": rock, "emissive": [0.0, 1e39, 0.0] }),
        json!({ "entity": rock, "alpha": 1e39 }),
    ] {
        assert!(
            agent
                .try_call("editor.set_render_override", params.clone())
                .is_err(),
            "{params}"
        );
    }
    assert!(
        agent
            .app
            .world()
            .get::<regia_render::RenderOverride>(entity)
            .is_none(),
        "no non-finite override was stored"
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// Audit v033b (Low): `editor.select` dropped elements that were not numbers, so
/// `["primary"]` became `[]` and cleared the selection with a success reply.
#[test]
fn select_refuses_ids_that_are_not_numbers() {
    let project = regia_test_utils::unique_temp_dir("editor-agent-select-ids");
    std::fs::create_dir_all(project.join("assets")).expect("assets dir");
    let mut agent = Agent::new(&project);
    let rock = agent.entity("editor.spawn", json!({ "name": "Rock" }));
    agent.call("editor.select", json!({ "entities": [rock], "mode": "set" }));
    let err = agent
        .try_call("editor.select", json!({ "entities": ["primary"] }))
        .expect_err("a string is not an entity id");
    assert_eq!(code(&err), "F0002", "{err:?}");
    let err = agent
        .try_call("editor.select", json!({ "entities": [rock, -1] }))
        .expect_err("a negative number is not an entity id");
    assert_eq!(code(&err), "F0002", "{err:?}");
    assert_eq!(
        agent.app.world().resource::<Selection>().len(),
        1,
        "the selection is unchanged"
    );
    // `[]` still clears.
    agent.call("editor.select", json!({ "entities": [] }));
    assert_eq!(agent.app.world().resource::<Selection>().len(), 0);
    let _ = std::fs::remove_dir_all(&project);
}

/// Audit v033b (Med): `editor.edit_material` edited a private copy: it accepted non-finite and
/// out-of-range PBR values, and overwrote a file whose unsaved panel edits would then drop the
/// agent's change on the panel's next save.
#[test]
fn edit_material_checks_values_and_respects_the_open_panel() {
    use regia_editor::material_editor::{MaterialPanelReload, MaterialPanelStatus};
    let project = regia_test_utils::unique_temp_dir("editor-agent-edit-material");
    std::fs::create_dir_all(project.join("assets/materials")).expect("assets dir");
    let file = project.join("assets/materials/hero.material.ron");
    let original = regia_asset::MaterialAssetRon::default_standard()
        .to_ron_string()
        .expect("ron");
    std::fs::write(&file, &original).expect("material");
    let mut agent = Agent::new(&project);
    agent
        .app
        .world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(project.clone()),
        });
    let path = "assets/materials/hero.material.ron";
    for params in [
        json!({ "path": path, "metallic": 1e39 }),
        json!({ "path": path, "roughness": -0.5 }),
        json!({ "path": path, "roughness": 3.0 }),
        json!({ "path": path, "base_color": [0.5, 1e39, 0.5, 1.0] }),
        json!({ "path": path, "base_color": [0.5, "red", 0.5] }),
    ] {
        assert!(
            agent.try_call("editor.edit_material", params.clone()).is_err(),
            "{params}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&file).expect("read"),
        original,
        "a refused call writes nothing"
    );

    // The panel has the file open with unsaved edits: refused with a named code.
    agent.app.world_mut().insert_resource(MaterialPanelStatus {
        path: Some(file.clone()),
        dirty: true,
    });
    let err = agent
        .try_call("editor.edit_material", json!({ "path": path, "metallic": 0.5 }))
        .expect_err("the panel's unsaved edits would overwrite the agent's");
    assert_eq!(
        err.data
            .as_ref()
            .and_then(|d| d.get("material_error"))
            .and_then(Value::as_str),
        Some("material_unsaved_in_panel"),
        "{err:?}"
    );
    assert_eq!(std::fs::read_to_string(&file).expect("read"), original);

    // A clean panel: the file is written and the panel is told to adopt it.
    agent.app.world_mut().insert_resource(MaterialPanelStatus {
        path: Some(file.clone()),
        dirty: false,
    });
    let out = agent.call("editor.edit_material", json!({ "path": path, "metallic": 0.5 }));
    assert_eq!(out["metallic"], json!(0.5));
    let reload = agent
        .app
        .world_mut()
        .remove_resource::<MaterialPanelReload>()
        .expect("the panel adopts the agent's save");
    assert!((reload.material.base.metallic - 0.5).abs() < 1e-6);
    let _ = std::fs::remove_dir_all(&project);
}

/// w6 item 4 (editor-guide finding): an entity the runtime methods `scene.spawn_primitive` and
/// `scene.spawn` made (no guid, no level) is written by `editor.save_level` with no shell frame
/// between the spawn and the save; a second save is byte-identical.
#[test]
fn runtime_spawns_are_saved_with_no_frame_between() {
    let project = regia_test_utils::unique_temp_dir("editor-agent-runtime-spawn");
    std::fs::create_dir_all(project.join("assets")).expect("assets dir");
    let mut agent = Agent::new(&project);
    agent.call(
        "scene.spawn_primitive",
        json!({ "shape": "sphere", "name": "Ball" }),
    );
    agent.call(
        "scene.spawn",
        json!({ "name": "Marker", "components": { "Transform": {
            "translation": [1.0, 0.0, 0.0], "rotation": [0.0, 0.0, 0.0, 1.0], "scale": [1.0, 1.0, 1.0] } } }),
    );
    let path = project.join("assets").join("runtime.level.ron");
    agent.call("editor.save_level", json!({ "path": path.display().to_string() }));
    let first = std::fs::read_to_string(&path).expect("saved level");
    assert!(
        first.contains("Ball") && first.contains("Marker"),
        "both runtime spawns are in the saved level: {first}"
    );
    agent.call("editor.save_level", json!({}));
    let second = std::fs::read_to_string(&path).expect("saved level");
    assert_eq!(first, second, "a second save is byte-identical");
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn history_jump_reports_touched_properties_and_replays_each_step() {
    let project = regia_test_utils::unique_temp_dir("undo-history-jump");
    let mut agent = Agent::new(&project);
    let entity = agent.entity("editor.spawn", json!({"name": "History"}));
    let before = agent.scene();
    agent.call(
        "editor.set_property",
        json!({"entity": entity, "component": "Transform", "path": "translation.x", "value": 4.0}),
    );
    let after = agent.scene();
    let history = agent.history();
    assert_eq!(history["entries"][1]["touched"][0]["entity"], entity);
    assert_eq!(
        history["entries"][1]["touched"][0]["property"],
        "Transform.translation.x"
    );
    agent.call("editor.history_jump", json!({"index": 1}));
    assert_eq!(agent.scene(), before);
    agent.call("editor.history_jump", json!({"index": 2}));
    assert_eq!(agent.scene(), after);
    assert!(
        agent
            .try_call("editor.history_jump", json!({"index": 3}))
            .is_err()
    );
}

#[test]
fn transactions_group_abort_and_refuse_nesting_through_dispatch() {
    let project = regia_test_utils::unique_temp_dir("undo-transactions");
    let mut agent = Agent::new(&project);
    let before = agent.scene();
    agent.call("editor.transaction_begin", json!({"label": "Build pair"}));
    let nested = agent
        .try_call("editor.transaction_begin", json!({"label": "Nested"}))
        .expect_err("nested");
    assert_eq!(
        nested.data.as_ref().expect("error data")["reason"],
        "transaction_nested"
    );
    let replace = agent
        .try_call("editor.command", json!({"run":"file.new_level_discard"}))
        .expect_err("cannot replace a transaction's world");
    assert_eq!(
        replace.data.as_ref().expect("error data")["reason"],
        "transaction_active"
    );
    agent.entity("editor.spawn", json!({"name": "One"}));
    agent.entity("editor.spawn", json!({"name": "Two"}));
    agent.call("editor.transaction_commit", json!({}));
    assert_eq!(agent.depth(), 1);
    assert_eq!(agent.history()["undo"][0], "Build pair");
    let after = agent.scene();
    agent.call("editor.undo", json!({}));
    assert_eq!(agent.scene(), before);
    agent.call("editor.redo", json!({}));
    assert_eq!(agent.scene(), after);
    agent.call("editor.transaction_begin", json!({"label": "Discard"}));
    agent.entity("editor.spawn", json!({"name": "Discarded"}));
    agent.call("editor.transaction_abort", json!({}));
    assert_eq!(agent.scene(), after);
    assert_eq!(agent.depth(), 1);
}

#[test]
fn hosted_scene_edits_join_the_editor_history() {
    let project = regia_test_utils::unique_temp_dir("undo-hosted-scene");
    let mut agent = Agent::new(&project);
    let before = agent.scene();
    agent.entity("scene.spawn", json!({"name": "Hosted"}));
    assert_eq!(agent.depth(), 1);
    agent.call("editor.undo", json!({}));
    assert_eq!(agent.scene(), before);
}

#[test]
fn vehicle_document_edits_use_the_shared_history() {
    let project = regia_test_utils::unique_temp_dir("undo-shared-vehicle");
    std::fs::create_dir_all(project.join("assets")).expect("assets");
    let mut agent = Agent::new(&project);
    agent
        .app
        .world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(project.clone()),
        });
    agent.call(
        "editor.vehicle_tuning_open",
        json!({"path": "assets/test.vehicle.ron", "create": true}),
    );
    agent.call(
        "editor.vehicle_tuning_set",
        json!({"field": "chassis_mass", "value": 1700.0}),
    );
    assert_eq!(agent.depth(), 1);
    assert_eq!(
        agent.history()["dirty"],
        false,
        "asset edits do not dirty the level"
    );
    let history = agent.history();
    assert_eq!(history["entries"][0]["documents"][0], "assets/test.vehicle.ron");
    assert_eq!(history["entries"][0]["touched"][0]["property"], "chassis_mass");
    agent.call("editor.undo", json!({}));
    let state = agent.call("editor.vehicle_tuning_state", json!({}));
    assert_ne!(state["config"]["chassis_mass"], 1700.0);
    let _ = std::fs::remove_dir_all(project);
}

#[test]
fn safe_mode_state_is_available_through_dispatch() {
    let project = regia_test_utils::unique_temp_dir("undo-safe-mode");
    let mut agent = Agent::new(&project);
    assert_eq!(agent.call("editor.safe_mode_state", json!({}))["enabled"], false);
    let denied = agent
        .try_call("editor.safe_mode_restart", json!({}))
        .expect_err("restart needs the tools permission");
    assert_eq!(denied.code, regia_agent::protocol::PERMISSION_DENIED);
    agent
        .app
        .world_mut()
        .resource_mut::<regia_agent::AgentPermissions>()
        .config
        .permissions
        .insert("tools".into());
    let refused = agent
        .try_call("editor.safe_mode_restart", json!({}))
        .expect_err("normal mode");
    assert_eq!(
        refused.data.as_ref().expect("error data")["reason"],
        "safe_mode_inactive"
    );
    agent
        .app
        .world_mut()
        .insert_resource(regia_editor::safe_mode::SafeMode::new(true, false));
    let state = agent.call("editor.safe_mode_state", json!({}));
    assert_eq!(state["enabled"], true);
    assert!(
        state["skipped"]
            .as_array()
            .expect("skipped array")
            .contains(&json!("agent_autostart"))
    );
    assert_eq!(
        agent.call("editor.safe_mode_restart", json!({}))["restart_requested"],
        true
    );
}

#[test]
fn table_document_edits_use_shared_history_and_reactivate_the_document() {
    let project = regia_test_utils::unique_temp_dir("undo-shared-tables");
    std::fs::create_dir_all(project.join("assets")).expect("assets");
    let curves = r#"(format_version:1, interp:Linear, curves:{"falloff":(keys:[(0.0,1.0)])})"#;
    std::fs::write(project.join("assets/a.curves.ron"), curves).expect("a");
    std::fs::write(project.join("assets/b.curves.ron"), curves).expect("b");
    let mut agent = Agent::new(&project);
    agent
        .app
        .world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(project.clone()),
        });
    agent.call("editor.tables_panel_open", json!({"path":"a.curves.ron"}));
    agent.call(
        "editor.tables_panel_curve_edit",
        json!({"op":"set_key", "name":"falloff", "x":2.0, "y":0.5}),
    );
    assert_eq!(agent.depth(), 1);
    assert_eq!(agent.history()["dirty"], false);
    agent.call("editor.tables_panel_open", json!({"path":"b.curves.ron"}));
    agent.call("editor.undo", json!({}));
    let state = agent.call("editor.tables_panel_get", json!({}));
    assert_eq!(state["curves"]["path"], "a.curves.ron");
    assert_eq!(
        state["curves"]["curves"][0]["keys"]
            .as_array()
            .expect("keys array")
            .len(),
        1
    );
    agent.call("editor.redo", json!({}));
    let state = agent.call("editor.tables_panel_get", json!({}));
    assert_eq!(
        state["curves"]["curves"][0]["keys"]
            .as_array()
            .expect("keys array")
            .len(),
        2
    );
    let _ = std::fs::remove_dir_all(project);
}

#[test]
fn closing_the_transport_commits_its_transaction_on_an_idle_frame() {
    use std::time::Duration;
    let _serial = TRANSPORT_WAKE_TEST.lock().expect("transport test lock");
    let project = regia_test_utils::unique_temp_dir("undo-connection-close");
    let mut agent = Agent::new(&project);
    let server = regia_agent::server::start("undo-close-test".into()).expect("server");
    let url = format!("ws://{}", server.addr);
    let (wakeup, wakes) = crossbeam_channel::unbounded();
    regia_agent::server::set_app_wake(Some(std::sync::Arc::new(move || {
        let _ = wakeup.send(());
    })));
    let (ready, received) = crossbeam_channel::bounded(1);
    let (release, close) = crossbeam_channel::bounded(1);
    let client = std::thread::spawn(move || {
        let mut client = regia_agent::client::Client::connect(&url, "undo-close-test").expect("connect");
        client
            .call("editor.transaction_begin", json!({"label":"Disconnected script"}))
            .expect("begin");
        client
            .call("editor.spawn", json!({"name":"Retained"}))
            .expect("spawn");
        ready.send(()).expect("ready");
        close.recv().expect("release");
        drop(client);
    });
    for _ in 0..2 {
        let inbound = server
            .inbox
            .recv_timeout(Duration::from_secs(30))
            .expect("request");
        agent.inbox.send(inbound).expect("dispatch");
        agent.app.update();
    }
    received.recv_timeout(Duration::from_secs(30)).expect("answered");
    let refused = agent
        .try_call("editor.spawn", json!({"name":"Other connection"}))
        .expect_err("owner");
    assert_eq!(
        refused.data.as_ref().expect("error data")["reason"],
        "transaction_owner"
    );
    assert_eq!(agent.history()["transaction"], "Disconnected script");
    while wakes.try_recv().is_ok() {}
    release.send(()).expect("close now");
    wakes
        .recv_timeout(Duration::from_secs(30))
        .expect("close wakes the host");
    client.join().expect("client");
    agent.app.update();
    regia_agent::server::set_app_wake(None);
    assert_eq!(agent.depth(), 1);
    assert!(agent.history()["transaction"].is_null());
    assert_eq!(agent.history()["undo"][0], "Disconnected script");
    agent.call("editor.undo", json!({}));
    assert_eq!(agent.depth(), 0);
}

#[test]
fn hosted_scene_operations_round_trip_and_batches_share_one_step() {
    let project = regia_test_utils::unique_temp_dir("undo-hosted-operations");
    let mut agent = Agent::new(&project);
    let root = agent.entity("scene.spawn", json!({"name":"Root"}));
    let child = agent.entity("scene.spawn", json!({"name":"Child"}));
    let before = agent.scene();
    agent.call(
        "scene.apply_edits",
        json!({"edits":[
            {"op":"add_component", "entity":child, "component":"UndoSmokeTag", "value":{"value":1.0}},
            {"op":"set_property", "entity":child, "component":"UndoSmokeTag", "path":"value", "value":7.0},
            {"op":"reparent", "entity":child, "parent":root}
        ]}),
    );
    assert_eq!(agent.depth(), 3);
    let after = agent.scene();
    agent.call("editor.undo", json!({}));
    assert_eq!(agent.scene(), before);
    agent.call("editor.redo", json!({}));
    assert_eq!(agent.scene(), after);
    agent.call(
        "scene.remove_component",
        json!({"entity":child,"component":"UndoSmokeTag"}),
    );
    agent.call("editor.undo", json!({}));
    assert_eq!(agent.scene(), after);
    agent.call("scene.despawn", json!({"entity":root}));
    agent.call("editor.undo", json!({}));
    assert_eq!(agent.scene(), after);
    // IDs held before the despawn still target the respawned child, including batch validation.
    let depth = agent.depth();
    agent.call(
        "scene.apply_edits",
        json!({"dry_run":true, "edits":[
            {"op":"set_property", "entity":child, "component":"UndoSmokeTag", "path":"value", "value":11.0}
        ]}),
    );
    assert_eq!(agent.depth(), depth);
    assert_eq!(agent.scene(), after);
    agent.call(
        "scene.set_property",
        json!({"entity":child,"component":"UndoSmokeTag","path":"value","value":12.0}),
    );
    agent.call("editor.undo", json!({}));
    assert_eq!(agent.scene(), after);
    agent.entity("scene.duplicate", json!({"entity":child}));
    agent.call("editor.undo", json!({}));
    assert_eq!(agent.scene(), after);
}

#[test]
fn explicit_transaction_groups_scene_and_vehicle_edits_and_abort_restores_both() {
    let project = regia_test_utils::unique_temp_dir("undo-mixed-transaction");
    std::fs::create_dir_all(project.join("assets")).expect("assets");
    let mut agent = Agent::new(&project);
    agent
        .app
        .world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(project.clone()),
        });
    agent.call(
        "editor.vehicle_tuning_open",
        json!({"path":"assets/mixed.vehicle.ron","create":true}),
    );
    let before_vehicle = agent.call("editor.vehicle_tuning_state", json!({}))["config"].clone();
    let before_scene = agent.scene();
    agent.call("editor.transaction_begin", json!({"label":"Mixed edit"}));
    agent.entity("scene.spawn", json!({"name":"Mixed"}));
    let changed = agent.call(
        "editor.vehicle_tuning_set",
        json!({"field":"chassis_mass","value":1800.0}),
    );
    assert_eq!(changed["undo_depth"], 0, "pending group is not a committed step");
    agent.call("editor.transaction_abort", json!({}));
    assert_eq!(agent.scene(), before_scene);
    assert_eq!(
        agent.call("editor.vehicle_tuning_state", json!({}))["config"],
        before_vehicle
    );
    assert_eq!(agent.depth(), 0);
    let _ = std::fs::remove_dir_all(project);
}

#[test]
fn runtime_world_without_the_editor_hook_does_not_record_scene_undo() {
    let project = regia_test_utils::unique_temp_dir("undo-play-world");
    let mut agent = Agent::new(&project);
    agent
        .app
        .world_mut()
        .remove_resource::<regia_agent::SceneEditHook>();
    agent.entity("scene.spawn", json!({"name":"Play entity"}));
    assert_eq!(agent.depth(), 0);
}

#[test]
fn history_panel_capture_changes_from_its_control_render() {
    let project = regia_test_utils::unique_temp_dir("undo-history-capture");
    std::fs::create_dir_all(&project).expect("project");
    let mut agent = Agent::new(&project);
    let params = json!({"panel":"undo_history", "width":720, "height":480});
    let control = agent.call("editor.capture_panel", params.clone());
    agent.entity("editor.spawn", json!({"name":"History capture marker"}));
    let edited = agent.call("editor.capture_panel", params.clone());
    assert_eq!(edited["mime"], "image/png");
    assert_eq!(edited["width"], 720);
    assert_eq!(edited["height"], 480);
    assert!(
        edited["png_base64"] != control["png_base64"],
        "history entries are drawn"
    );
    assert!(
        agent.call("editor.capture_panel", params)["png_base64"] == edited["png_base64"],
        "stable control render"
    );
    if std::env::var_os("REGIA_UNDO_EVIDENCE").is_some() {
        let pending = project.join("history-capture.json.tmp");
        let path = project.join("history-capture.json");
        std::fs::write(&pending, serde_json::to_vec(&edited).expect("json")).expect("write");
        std::fs::rename(&pending, &path).expect("publish");
        eprintln!("UNDO_HISTORY_EVIDENCE={}", path.display());
    } else {
        let _ = std::fs::remove_dir_all(project);
    }
}

#[test]
fn hosted_data_table_inverses_round_trip_cells_and_row_structure() {
    let project = regia_test_utils::unique_temp_dir("undo-data-inverses");
    std::fs::create_dir_all(project.join("assets")).expect("assets");
    std::fs::write(
        project.join("assets/values.table.ron"),
        r#"(format_version:1, row_type:"HistoryRow", rows:{"a":(value:1),"b":(value:2)})"#,
    )
    .expect("fixture");
    let mut agent = Agent::new(&project);
    agent
        .app
        .world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(project.clone()),
        });
    let opened = agent.call("editor.tables_panel_open", json!({"path":"values.table.ron"}));
    let mut rows = vec![opened["data"]["rows"].clone()];
    for edit in [
        json!({"op":"set_cell","key":"a","column":"value","ron":"9"}),
        json!({"op":"rename_row","key":"a","to":"renamed"}),
        json!({"op":"move_row","key":"renamed"}),
        json!({"op":"remove_row","key":"b"}),
        json!({"op":"insert_row","key":"c","cells":{"value":3}}),
    ] {
        let state = agent.call("editor.tables_panel_data_edit", edit);
        rows.push(state["data"]["rows"].clone());
    }
    assert_eq!(agent.depth(), 5);
    for expected in rows[..5].iter().rev() {
        agent.call("editor.undo", json!({}));
        let state = agent.call("editor.tables_panel_get", json!({}));
        assert_eq!(&state["data"]["rows"], expected);
        assert_eq!(state["data"]["dirty"], true);
    }
    for expected in &rows[1..] {
        agent.call("editor.redo", json!({}));
        let state = agent.call("editor.tables_panel_get", json!({}));
        assert_eq!(&state["data"]["rows"], expected);
    }
    assert_eq!(agent.history()["dirty"], false);
    let _ = std::fs::remove_dir_all(project);
}

#[test]
fn refused_hosted_spawn_does_not_leave_an_entity_outside_history() {
    fn scene_count(world: &mut World) -> usize {
        world
            .query_filtered::<Entity, Without<regia_ecs::ecs::resource::IsResource>>()
            .iter(world)
            .count()
    }
    let project = regia_test_utils::unique_temp_dir("undo-refused-spawn");
    let mut agent = Agent::new(&project);
    let entities = scene_count(agent.app.world_mut());
    agent
        .try_call("scene.spawn", json!({"name":42}))
        .expect_err("invalid name");
    assert_eq!(scene_count(agent.app.world_mut()), entities);
    assert_eq!(agent.depth(), 0);
}

#[test]
fn hosted_spawn_uses_the_editors_serializable_name_through_undo_and_redo() {
    let project = regia_test_utils::unique_temp_dir("undo-hosted-name");
    let mut agent = Agent::new(&project);
    let entity = agent.entity("scene.spawn", json!({"name":"Before"}));
    agent.call(
        "scene.set_property",
        json!({
            "entity":entity, "component":"regia_scene::components::Name", "path":"", "value":"After"
        }),
    );
    assert_eq!(
        agent.call("scene.get_entity", json!({"entity":entity}))["name"],
        "After"
    );
    agent.call("editor.undo", json!({}));
    assert_eq!(
        agent.call("scene.get_entity", json!({"entity":entity}))["name"],
        "Before"
    );
    agent.call("editor.undo", json!({}));
    agent.call("editor.redo", json!({}));
    agent.call("editor.redo", json!({}));
    let current = regia_editor::command::resolve_entity(agent.app.world(), entity).expect("respawned");
    assert_eq!(
        agent.call("scene.get_entity", json!({"entity":current.to_bits()}))["name"],
        "After"
    );
}

#[test]
fn hosted_pending_prefab_keeps_its_level_when_loaded_after_redo() {
    use regia_core::id::Guid;
    use regia_scene::prefab::{PendingPrefab, PrefabLibrary, PrefabTemplate};
    use regia_scene::prelude::{EntityGuid, LevelMember};
    let project = regia_test_utils::unique_temp_dir("undo-pending-prefab");
    let mut agent = Agent::new(&project);
    let prefab = Guid::from_u128(7826);
    let root = agent.entity("scene.spawn_prefab", json!({"prefab":prefab.to_string()}));
    assert_eq!(agent.depth(), 1);
    agent.call("editor.undo", json!({}));
    agent.call("editor.redo", json!({}));
    let current = regia_editor::command::resolve_entity(agent.app.world(), root).expect("respawned root");
    assert_eq!(
        agent
            .app
            .world()
            .get::<PendingPrefab>(current)
            .expect("pending")
            .level,
        agent.level.0.as_u128()
    );
    let file = regia_scene::format::SceneFile::from_ron_str(r#"(format_version:1, entities:[
        (guid:"00000000-0000-0000-0000-0000000000e1", name:"Root"),
        (guid:"00000000-0000-0000-0000-0000000000e2", name:"Child", parent:Some("00000000-0000-0000-0000-0000000000e1")),
    ])"#).expect("prefab file");
    let registry = agent
        .app
        .world()
        .resource::<regia_ecs::reflect::AppTypeRegistry>()
        .clone();
    let template = PrefabTemplate::from_scene_file(prefab, &file, &registry.read()).expect("template");
    agent
        .app
        .world_mut()
        .resource_mut::<PrefabLibrary>()
        .insert(template);
    agent.app.update();
    assert!(agent.app.world().get::<PendingPrefab>(current).is_none());
    let world = agent.app.world_mut();
    let entities: Vec<_> = world
        .query_filtered::<Entity, With<EntityGuid>>()
        .iter(world)
        .collect();
    assert!(entities.len() >= 2, "template child resolved");
    for entity in entities {
        assert_eq!(
            world.get::<LevelMember>(entity).map(LevelMember::level),
            Some(agent.level)
        );
    }
    let after = agent.scene();
    agent.call("editor.undo", json!({}));
    agent.call("editor.redo", json!({}));
    assert_eq!(agent.scene(), after);
}

static TRANSPORT_WAKE_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn hosted_season_edits_undo_but_queries_do_not_add_entries() {
    use regia_framework::season::SeasonState;
    let project = regia_test_utils::unique_temp_dir("undo-season");
    let mut agent = Agent::new(&project);
    agent.app.world_mut().insert_resource(SeasonState::default());
    let before = agent.call("scene.season", json!({}))["after"].clone();
    assert_eq!(agent.depth(), 0);
    agent.call(
        "scene.season",
        json!({"season":"winter", "hold_summer":["grass"]}),
    );
    assert_eq!(agent.depth(), 1);
    let history = agent.history();
    assert_eq!(history["dirty"], false, "season state is not a level-file edit");
    assert_eq!(history["entries"][0]["documents"], json!(["session"]));
    let after = agent.call("scene.season", json!({}))["after"].clone();
    assert_eq!(agent.depth(), 1);
    agent.call("editor.undo", json!({}));
    assert_eq!(agent.call("scene.season", json!({}))["after"], before);
    agent.call("editor.redo", json!({}));
    assert_eq!(agent.call("scene.season", json!({}))["after"], after);
}

#[test]
fn hosted_possession_restores_both_targets_and_uses_remapped_entities() {
    use regia_framework::player::PlayerController;
    let project = regia_test_utils::unique_temp_dir("undo-possession");
    let mut agent = Agent::new(&project);
    agent.app.register_type::<PlayerController>();
    let controller = agent.entity("editor.spawn", json!({"name":"Controller"}));
    let pawn = agent.entity("editor.spawn", json!({"name":"Pawn"}));
    agent
        .app
        .world_mut()
        .entity_mut(Entity::try_from_bits(controller).expect("controller"))
        .insert(PlayerController::default());
    agent.call("scene.possess", json!({"controller":controller, "entity":pawn}));
    assert_eq!(agent.depth(), 3);
    agent.call("editor.undo", json!({}));
    let current = regia_editor::command::resolve_entity(agent.app.world(), controller).expect("controller");
    let pc = agent
        .app
        .world()
        .get::<PlayerController>(current)
        .expect("component");
    assert_eq!(pc.possessed, Entity::PLACEHOLDER);
    assert_eq!(pc.view_target, Entity::PLACEHOLDER);
    agent.call("editor.undo", json!({})); // Undo the pawn spawn, then redo with a new id.
    agent.call("editor.redo", json!({}));
    agent.call("editor.redo", json!({}));
    let pawn = regia_editor::command::resolve_entity(agent.app.world(), pawn).expect("pawn");
    let pc = agent
        .app
        .world()
        .get::<PlayerController>(current)
        .expect("component");
    assert_eq!((pc.possessed, pc.view_target), (pawn, pawn));
}

#[test]
fn shared_history_methods_are_discoverable_in_the_agent_guide() {
    let project = regia_test_utils::unique_temp_dir("undo-guide");
    let mut agent = Agent::new(&project);
    let guide = agent.call("agent.guide", json!({"topic":"undo"}));
    let text = guide["text"].as_str().expect("guide text");
    for method in [
        "editor.history_jump",
        "editor.transaction_begin",
        "editor.transaction_commit",
        "editor.transaction_abort",
        "editor.safe_mode_state",
        "editor.safe_mode_restart",
    ] {
        assert!(regia_agent::mentions_token(text, method), "{method}");
    }
}

#[cfg(unix)]
#[test]
fn closed_notifications_keep_their_transaction_until_the_queue_is_drained() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    fn write(stream: &mut UnixStream, value: Value) {
        let body = serde_json::to_vec(&value).expect("json");
        stream
            .write_all(&(body.len() as u32).to_le_bytes())
            .expect("header");
        stream.write_all(&body).expect("body");
    }
    let _serial = TRANSPORT_WAKE_TEST.lock().expect("transport test lock");
    let project = regia_test_utils::unique_temp_dir("undo-notes");
    let mut agent = Agent::new(&project);
    let before = agent.scene();
    let server = regia_agent::local::start("queued-test".into(), &project, None)
        .expect("local server")
        .expect("Unix transport");
    let mut client = UnixStream::connect(&server.address).expect("connect");
    client
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("timeout guard");
    write(&mut client, json!({"bearer":"queued-test"}));
    let mut header = [0; 4];
    client.read_exact(&mut header).expect("handshake header");
    let mut body = vec![0; u32::from_le_bytes(header) as usize];
    client.read_exact(&mut body).expect("handshake");
    assert_eq!(body, regia_agent::local::HANDSHAKE_ACCEPTED.as_bytes());
    let (wakeup, wakes) = crossbeam_channel::bounded(1);
    regia_agent::server::set_app_wake(Some(std::sync::Arc::new(move || {
        let _ = wakeup.try_send(());
    })));
    for (method, params) in [
        ("editor.transaction_begin", json!({"label":"Queued script"})),
        ("editor.spawn", json!({"name":"Queued A"})),
        ("editor.spawn", json!({"name":"Queued B"})),
    ] {
        write(
            &mut client,
            json!({"jsonrpc":"2.0","method":method,"params":params}),
        );
        let request = server
            .inbox
            .recv_timeout(Duration::from_secs(30))
            .expect("queued");
        agent.inbox.send(request).expect("host inbox");
        wakes
            .recv_timeout(Duration::from_secs(30))
            .expect("queued request wakes the host");
    }
    drop(client);
    wakes
        .recv_timeout(Duration::from_secs(30))
        .expect("connection closed");
    agent.app.update();
    regia_agent::server::set_app_wake(None);
    assert_eq!(
        agent.depth(),
        1,
        "all queued edits commit as one group after close"
    );
    assert!(agent.history()["transaction"].is_null());
    assert_eq!(agent.history()["undo"][0], "Queued script");
    agent.call("editor.undo", json!({}));
    assert_eq!(agent.scene(), before);
    drop(server);
    let _ = std::fs::remove_dir_all(project);
}
