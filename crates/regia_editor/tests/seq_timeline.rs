// SPDX-License-Identifier: MPL-2.0
//! The sequencer timeline through the real agent dispatcher (ADR-7322, `book/agents/seq-curve-panel.md`):
//! box-select keys on three tracks and move them +0.5 s (one undo entry, undone by `editor.undo`),
//! add a marker and a camera cut at the playhead, play the transport, and read it all back from
//! `editor.seq_timeline_state`. Requests come from an in-process inbox, each answered within one
//! frame (no wall-clock waits).

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::level_io;
use regia_editor::selection::Selection;
use regia_framework::sequencer::{Channel, Ease, ScalarKey, Sequence, SequencePlayer, Track};
use regia_render::Camera;
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

struct Agent {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
}

impl Agent {
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
        app.insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(project.to_path_buf()),
        });
        let open = level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        regia_editor::agent_ext::install(app.world_mut());
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

fn code(err: &RpcError) -> &str {
    err.data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn float_track(name: &str, keys: &[(f32, f32)]) -> Track {
    Track {
        name: name.into(),
        target: "ui".into(),
        property: name.into(),
        keys: Channel::Float(
            keys.iter()
                .map(|&(t, v)| ScalarKey::new(t, v, Ease::Bezier))
                .collect(),
        ),
        enabled: true,
        locked: false,
    }
}

#[test]
fn the_timeline_edits_keys_markers_cuts_and_plays_through_the_dispatcher() {
    let project = regia_test_utils::unique_temp_dir("seq-timeline-e2e");
    let mut agent = Agent::new(&project);
    let sequence = Sequence {
        fps: 30.0,
        tracks: vec![
            float_track("a", &[(0.0, 0.0), (1.0, 1.0)]),
            float_track("b", &[(0.0, 0.0), (1.0, 2.0)]),
            float_track("c", &[(0.0, 0.0), (1.0, 3.0)]),
        ],
        ..Sequence::default()
    };
    let player = agent.app.world_mut().spawn(SequencePlayer::new(sequence)).id();
    let camera = agent
        .app
        .world_mut()
        .spawn((Transform::default(), Camera::default()))
        .id();

    // Box-select the 1 s keys on all three tracks and move them +0.5 s.
    let picked = agent.call("editor.seq_keys", json!({ "op": "select", "range": [0.5, 1.5] }));
    assert_eq!(picked["selection"].as_array().map(Vec::len), Some(3));
    let depth = agent.call("editor.history", json!({}))["undo_depth"]
        .as_u64()
        .unwrap_or(0);
    agent.call("editor.seq_keys", json!({ "op": "move", "delta": 0.5 }));
    let state = agent.call("editor.seq_timeline_state", json!({}));
    for row in state["rows"].as_array().expect("rows") {
        assert_eq!(row["keys"], json!([0.0, 1.5]), "{row}");
    }
    let sampled = agent
        .app
        .world()
        .get::<SequencePlayer>(player)
        .and_then(|p| p.sequence().tracks[2].keys.sample_component(1.5, 0));
    assert_eq!(sampled, Some(3.0), "the sampler agrees");
    assert_eq!(
        agent.call("editor.history", json!({}))["undo_depth"].as_u64(),
        Some(depth + 1),
        "one undo entry"
    );
    agent.call("editor.undo", json!({}));
    let state = agent.call("editor.seq_timeline_state", json!({}));
    assert_eq!(state["rows"][0]["keys"], json!([0.0, 1.0]), "undone");

    // A refused edit names its code and changes nothing.
    let err = agent
        .try_call(
            "editor.seq_keys",
            json!({ "op": "move", "delta": -2.0, "keys": [{ "track": "a", "index": 1 }] }),
        )
        .expect_err("negative time");
    assert_eq!(code(&err), "seq_keys_out_of_range");

    // A marker and a camera cut at the playhead (0.5 s).
    agent.call("editor.seq_transport", json!({ "op": "seek", "time": 0.5 }));
    let marked = agent.call(
        "editor.seq_marker_edit",
        json!({ "op": "add", "name": "hit", "payload": "boom" }),
    );
    assert_eq!(
        marked["markers"][0],
        json!({ "name": "hit", "time": 0.5, "payload": "boom" })
    );
    let cut = agent.call(
        "editor.seq_cut_edit",
        json!({ "op": "add", "camera": camera.to_bits() }),
    );
    assert_eq!(cut["cuts"]["cuts"], json!([0.5]));

    // Play: a tick advances the clock the way each drawn frame does.
    agent.call("editor.seq_transport", json!({ "op": "play" }));
    let played = agent.call("editor.seq_transport", json!({ "op": "tick", "dt": 0.25 }));
    assert_eq!(played["time"], json!(0.75));
    assert_eq!(played["timecode"], json!("00:00:00:23"));
    let state = agent.call("editor.seq_timeline_state", json!({}));
    assert_eq!(state["transport"]["playing"], json!(true));
    assert_eq!(state["markers"][0]["name"], json!("hit"));
    let _ = std::fs::remove_dir_all(&project);
}

/// ADR-8640, ripple edit: a rippling trim through the dispatcher is one `editor.history` entry,
/// and one `editor.undo` restores the sequence's keys and markers and the shot list together.
#[test]
fn a_ripple_trim_through_the_dispatcher_is_one_undo_entry() {
    let project = regia_test_utils::unique_temp_dir("seq-ripple-e2e");
    let mut agent = Agent::new(&project);
    let mut sequence = Sequence {
        fps: 30.0,
        tracks: vec![
            float_track("a", &[(0.5, 0.0), (1.0, 1.0), (4.5, 2.0), (5.5, 3.0)]),
            float_track("b", &[(0.0, 0.0), (1.0, 2.0)]),
        ],
        ..Sequence::default()
    };
    sequence.markers = vec![
        regia_framework::sequencer::Marker {
            name: "stays".into(),
            time: 3.4,
            payload: String::new(),
        },
        regia_framework::sequencer::Marker {
            name: "late".into(),
            time: 5.0,
            payload: String::new(),
        },
    ];
    let player = agent.app.world_mut().spawn(SequencePlayer::new(sequence)).id();
    let camera = agent
        .app
        .world_mut()
        .spawn((Transform::default(), Camera::default()))
        .id();
    // Three cuts' worth of shots on the player's own list.
    agent.call(
        "editor.seq_cut_edit",
        json!({ "op": "add", "time": 0.5, "camera": camera.to_bits() }),
    );
    agent.call("editor.seq_cut_edit", json!({ "op": "add", "time": 2.0 }));
    agent.call("editor.seq_cut_edit", json!({ "op": "add", "time": 4.0 }));
    let before = agent.call("editor.seq_timeline_state", json!({}));

    let depth = agent.call("editor.history", json!({}))["undo_depth"]
        .as_u64()
        .unwrap_or(0);
    let trimmed = agent.call(
        "editor.seq_cut_edit",
        json!({ "op": "trim", "shot": 1, "duration": 1.0, "ripple": true }),
    );
    assert_eq!(trimmed["cuts"]["cuts"], json!([0.5, 1.5, 3.5]));
    let state = agent.call("editor.seq_timeline_state", json!({}));
    assert_eq!(
        state["rows"][0]["keys"],
        json!([0.5, 1.0, 4.0, 5.0]),
        "every key after the edit point shifted 0.5 s earlier"
    );
    assert_eq!(
        state["markers"],
        json!([
            // The marker time is an f32 on the wire (2.9f32 widens to 2.9000000953674316).
            { "name": "stays", "time": f64::from(2.9_f32), "payload": "" },
            { "name": "late", "time": 4.5, "payload": "" },
        ]),
    );
    assert_eq!(
        agent.call("editor.history", json!({}))["undo_depth"].as_u64(),
        Some(depth + 1),
        "one undo entry for the whole ripple"
    );
    // The sampler agrees at a probe inside the shifted span (4.0 s holds what 4.5 s held).
    assert_eq!(
        agent
            .app
            .world()
            .get::<SequencePlayer>(player)
            .and_then(|p| p.sequence().tracks[0].keys.sample_component(4.0, 0)),
        Some(2.0),
    );
    agent.call("editor.undo", json!({}));
    let after = agent.call("editor.seq_timeline_state", json!({}));
    assert_eq!(
        after["rows"][0]["keys"], before["rows"][0]["keys"],
        "keys restored"
    );
    assert_eq!(after["markers"], before["markers"], "markers restored");
    assert_eq!(after["cuts"]["cuts"], before["cuts"]["cuts"], "cuts restored");
    let _ = std::fs::remove_dir_all(&project);
}
