// SPDX-License-Identifier: MPL-2.0
//! Dispatcher acceptance fixtures for review notes and Activity.
use regia_agent::{
    protocol::{Request, RpcError},
    server::Inbound,
};
use regia_app::prelude::*;
use serde_json::{Value, json};

pub(crate) struct Agent {
    pub(crate) app: App,
    inbox: crossbeam_channel::Sender<Inbound>,
    root: std::path::PathBuf,
}

impl Agent {
    pub(crate) fn new() -> Self {
        Self::with_history("")
    }

    pub(crate) fn with_history(history: &str) -> Self {
        let root = regia_test_utils::unique_temp_dir("review-notes");
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        if !history.is_empty() {
            let log = root.join(".regia/agent/audit.jsonl");
            std::fs::create_dir_all(log.parent().expect("agent directory")).expect("directory");
            regia_test_utils::write_atomic(&log, history.as_bytes()).expect("historical audit");
        }
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.clone()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.init_resource::<crate::command::UndoStack>();
        app.init_resource::<crate::selection::Selection>();
        app.init_resource::<crate::viewport::EditorCamera>();
        let open = crate::level_io::new_level(app.world_mut());
        app.insert_resource(open);
        crate::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded();
        app.insert_resource(regia_agent::AgentInbox(requests));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = u32::MAX;
        app.insert_resource(permissions);
        Self { app, inbox, root }
    }

    pub(crate) fn try_call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        let (reply, response) = crossbeam_channel::bounded(1);
        self.inbox
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply,
            })
            .expect("inbox");
        self.app.update();
        let response = response.try_recv().expect("one deterministic dispatch frame");
        if let Some(error) = response.error {
            Err(error)
        } else {
            Ok(response.result.expect("result"))
        }
    }

    pub(crate) fn call(&mut self, method: &str, params: Value) -> Value {
        self.try_call(method, params)
            .unwrap_or_else(|error| panic!("{method}: {error:?}"))
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        // Close the audit handle before directory cleanup on Windows too.
        drop(self.app.world_mut().remove_resource::<regia_agent::AuditLog>());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn review_notes_dispatch_save_load_reply_resolve_and_undo() {
    let mut agent = Agent::new();
    let parent = agent.call(
        "editor.spawn",
        json!({"name": "Reviewed item", "translation": [2,3,4]}),
    )["entity"]
        .as_u64()
        .expect("parent");
    let note = agent.call(
        "editor.note_add",
        json!({"entity": parent, "text": "Check clearance"}),
    );
    let entity = note["entity"].as_u64().expect("marker");
    assert_ne!(
        entity, parent,
        "dedicated marker, never annotate the shipping entity itself"
    );
    agent.call("editor.note_reply", json!({"entity": entity, "text": "Adjusted"}));
    agent.call("editor.undo", json!({}));
    assert_eq!(
        agent.call("editor.note_list", json!({}))["notes"][0]["note"]["thread"],
        json!([])
    );
    agent.call("editor.redo", json!({}));
    assert_eq!(
        agent.call("editor.note_list", json!({}))["notes"][0]["note"]["thread"]
            .as_array()
            .expect("replies")
            .len(),
        1
    );
    agent.call("editor.note_resolve", json!({"entity": entity, "resolved": true}));
    assert_eq!(
        agent.call("editor.note_list", json!({"filter": "open"}))["notes"],
        json!([])
    );
    let rows = agent.call("editor.note_list", json!({"filter": "resolved"}));
    assert_eq!(rows["notes"][0]["note"]["thread"][0]["text"], "Adjusted");
    let path = agent.root.join("assets/review.level.ron");
    let open = agent.app.world().resource::<crate::level_io::OpenLevel>().clone();
    crate::level_io::save_level_file(agent.app.world_mut(), &open, &path).expect("save");
    let saved = std::fs::read_to_string(&path).expect("read");
    assert!(saved.contains("Check clearance") && saved.contains("Adjusted"));
    let reopened = crate::level_io::open_level_file(agent.app.world_mut(), &path).expect("open");
    agent.app.insert_resource(reopened);
    let loaded = agent.call("editor.note_list", json!({}));
    assert_eq!(loaded["notes"].as_array().expect("rows").len(), 1);
    assert_eq!(loaded["notes"][0]["note"], rows["notes"][0]["note"]);
    let loaded_id = loaded["notes"][0]["entity"].as_u64().expect("id");
    agent.call(
        "editor.note_resolve",
        json!({"entity": loaded_id, "resolved": false}),
    );
    assert_eq!(
        agent.call("editor.note_list", json!({"filter": "open"}))["notes"]
            .as_array()
            .expect("rows")
            .len(),
        1
    );
    agent.call("editor.undo", json!({}));
    assert_eq!(
        agent.call("editor.note_list", json!({"filter": "resolved"}))["notes"]
            .as_array()
            .expect("rows")
            .len(),
        1
    );
}

#[test]
fn review_notes_rejected_calls_change_nothing_and_surface_position_round_trips() {
    let mut agent = Agent::new();
    for params in [
        json!({"text": " "}),
        json!({"text":"valid","position":[1,2]}),
        json!({"text":"valid","entity":0}),
    ] {
        assert!(agent.try_call("editor.note_add", params).is_err());
    }
    assert_eq!(agent.call("editor.note_list", json!({}))["notes"], json!([]));
    let result = agent.call("editor.note_add", json!({"text":"Surface", "position":[1,2,3]}));
    let entity = result["entity"].as_u64().expect("entity");
    assert!(
        agent
            .try_call("editor.note_reply", json!({"entity":entity,"text":""}))
            .is_err()
    );
    assert!(
        agent
            .try_call("editor.note_resolve", json!({"entity":entity,"resolved":"yes"}))
            .is_err()
    );
    let list = agent.call("editor.note_list", json!({}));
    assert_eq!(list["notes"][0]["position"], json!([1.0, 2.0, 3.0]));
    agent.call("editor.undo", json!({}));
    assert_eq!(agent.call("editor.note_list", json!({}))["notes"], json!([]));
    agent.call("editor.redo", json!({}));
    assert_eq!(
        agent.call("editor.note_list", json!({}))["notes"][0]["note"]["text"],
        "Surface"
    );
}

#[test]
fn activity_panel_agent_call_then_panel_edit_are_ordered_and_selectable() {
    use crate::command::EditorCommand;
    let mut agent = Agent::new();
    agent.call("agent.hello", json!({"client":"review-tester"}));
    let entity = agent.call("editor.spawn", json!({"name":"Reviewed item"}))["entity"]
        .as_u64()
        .expect("entity");
    let world = agent.app.world_mut();
    let id = crate::command::resolve_entity(world, entity).expect("live entity");
    // This is the same command and undo-stack path used by the inspector/gizmo panel.
    let mut edit = crate::command::SetTransform::capture(
        world,
        id,
        regia_scene::prelude::Transform {
            translation: regia_core::math::Vec3::X,
            ..Default::default()
        },
    );
    edit.apply(world);
    world
        .resource_mut::<crate::command::UndoStack>()
        .push(Box::new(edit));
    let timeline = agent.call("editor.activity", json!({"since":0}));
    let entries = timeline["entries"].as_array().expect("entries");
    let call = entries
        .iter()
        .position(|row| row["method"] == "editor.spawn")
        .expect("agent call");
    let edit = entries
        .iter()
        .position(|row| row["source"] == "user")
        .expect("panel edit");
    assert!(call < edit, "{timeline}");
    assert_eq!(entries[call]["who"], "review-tester");
    assert_eq!(entries[edit]["entities"], json!([entity]));
    assert!(
        entries
            .windows(2)
            .all(|pair| pair[0]["seq"].as_u64() < pair[1]["seq"].as_u64())
    );
    assert!(
        agent.call("editor.activity", json!({"source":"user","method":"Transform"}))["entries"]
            .as_array()
            .expect("filtered")
            .iter()
            .all(|row| row["source"] == "user")
    );
    agent.call("editor.select", json!({"entities":[entity]}));
    assert_eq!(
        agent
            .app
            .world()
            .resource::<crate::selection::Selection>()
            .primary(),
        Some(id)
    );
}
