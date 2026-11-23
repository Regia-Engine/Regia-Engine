// SPDX-License-Identifier: MPL-2.0
//! The smart-object definition editor over the agent protocol (rule 8, ADR-7736): every call goes
//! through the real dispatcher (registration, parameter schema, permissions), fed from an in-process
//! inbox so each one is answered within one frame. Content ▸ New makes the file, the editor opens and
//! edits it, the slot handles move a slot, undo and save behave, and the runtime loader reads the
//! result.

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_ai::smart_object_def::SmartObjectDef;
use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::selection::Selection;
use regia_scene::GlobalTransform;
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

struct Client {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
}

impl Client {
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
        regia_editor::agent_ext::install(app.world_mut());
        app.world_mut()
            .insert_resource(regia_editor::agent_ext::EditorProject {
                root: Some(project.to_path_buf()),
            });
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded::<Inbound>();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        app.world_mut().insert_resource(permissions);
        Self {
            app,
            inbox,
            next_id: 0,
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
}

fn reason(err: &RpcError) -> &str {
    err.data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

#[test]
fn a_client_creates_edits_places_and_saves_a_definition() {
    let project = regia_test_utils::unique_temp_dir("smart_object_def_agent");
    let mut client = Client::new(&project);
    let rel = "assets/props/bench.smartobject.ron";

    // Content ▸ New ▸ Smart object.
    client.call(
        "editor.create_asset",
        json!({ "kind": "smart_object", "path": rel }),
    );
    let created = std::fs::read_to_string(project.join(rel)).expect("created");
    let def = SmartObjectDef::from_ron(&created).expect("the loader reads the new file");
    assert_eq!(def.slots.len(), 1);

    let state = client.call("editor.smart_object_def_state", json!({}));
    assert_eq!(state["open"], false);
    let out = client.call(
        "editor.smart_object_def_edit",
        json!({ "path": rel, "op": "set_activity", "tags": ["Activity.Sit"] }),
    );
    assert_eq!(out["changed"], true);
    assert_eq!(out["dirty"], true);
    client.call(
        "editor.smart_object_def_edit",
        json!({ "op": "add_slot", "name": "right", "translation": [0.4, 0.0, 0.0] }),
    );
    client.call(
        "editor.smart_object_def_edit",
        json!({ "op": "add_entrance", "slot": "right", "translation": [0.0, 0.0, 1.0], "kind": "enter" }),
    );

    // The slot handles on a preview entity at x = 5: drag slot 1 one metre along +Z.
    let entity = {
        let world = client.app.world_mut();
        let t = Transform::from_translation(regia_core::math::Vec3::new(5.0, 0.0, 0.0));
        world.spawn((t, GlobalTransform(t.compute_affine()))).id()
    };
    let listed = client.call(
        "editor.smart_object_slot_handles",
        json!({ "preview": entity.to_bits(), "entity": entity.to_bits() }),
    );
    let handles = listed["handles"].as_array().expect("handles");
    let right = handles.iter().find(|h| h["name"] == "slot_1").expect("slot_1");
    let at: Vec<f64> = right["position"]
        .as_array()
        .expect("position")
        .iter()
        .map(|v| v.as_f64().expect("number"))
        .collect();
    assert!(
        (at[0] - 5.4).abs() < 1e-5 && at[1].abs() < 1e-6 && at[2].abs() < 1e-6,
        "{at:?}"
    );
    let dragged = client.call(
        "editor.smart_object_slot_handle_drag",
        json!({ "entity": entity.to_bits(), "handle": "slot_1", "to": [5.4, 0.0, 1.0] }),
    );
    assert_eq!(dragged["changed"], true);
    let offset = &dragged["slots"][1]["offset"]["translation"];
    assert!((offset[2].as_f64().expect("z") - 1.0).abs() < 1e-5, "{offset}");

    // Undo through the shared stack takes the drag back as one step.
    let depth = dragged["undo_depth"].as_u64().expect("depth");
    client.call("editor.undo", json!({}));
    let state = client.call("editor.smart_object_def_state", json!({}));
    assert_eq!(state["undo_depth"].as_u64(), Some(depth - 1));
    let back = &state["slots"][1]["offset"]["translation"];
    assert!(
        (back[0].as_f64().expect("x") - 0.4).abs() < 1e-6 && back[2].as_f64() == Some(0.0),
        "{back}"
    );

    // An invalid definition is refused at save with the validator's reason.
    client.call(
        "editor.smart_object_def_edit",
        json!({ "op": "set_slot", "slot": "right", "name": "slot_0" }),
    );
    let refused = client
        .try_call("editor.smart_object_def_edit", json!({ "op": "save" }))
        .expect_err("duplicate name");
    assert_eq!(reason(&refused), "smart_object_def_invalid");
    client.call("editor.undo", json!({}));

    let saved = client.call("editor.smart_object_def_edit", json!({ "op": "save" }));
    assert!(saved["written"].as_u64().is_some_and(|n| n > 0));
    let bytes = std::fs::read_to_string(project.join(rel)).expect("saved");
    let def = SmartObjectDef::from_ron(&bytes).expect("the loader reads the saved file");
    assert_eq!(def.activity, vec!["Activity.Sit".to_owned()]);
    assert_eq!(def.slots[1].name, "right");
    assert_eq!(def.slots[1].entrances.len(), 1);
    assert_eq!(bytes, def.to_ron().expect("ron"), "the canonical bytes");

    // Errors keep their codes through the dispatcher.
    let e = client
        .try_call(
            "editor.smart_object_def_edit",
            json!({ "op": "remove_slot", "slot": 9 }),
        )
        .expect_err("missing slot");
    assert_eq!(reason(&e), "smart_object_def_bad_index");
    let e = client
        .try_call(
            "editor.smart_object_def_edit",
            json!({ "path": "assets/../x.smartobject.ron" }),
        )
        .expect_err("outside");
    assert_eq!(reason(&e), "smart_object_def_bad_path");
}
