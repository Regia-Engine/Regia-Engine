// SPDX-License-Identifier: MPL-2.0
//! Plain-node clips are inspectable through the real agent dispatcher (ADR-8157).

use regia_agent::{LocalReply, call_local};
use regia_animation::node_tracks::NodeAnimation;
use regia_animation::{AnimationClip, Animator};
use regia_app::App;
use serde_json::{Value, json};

fn call(app: &mut App, method: &str, params: Value) -> Value {
    match call_local(app.world_mut(), method, params, None) {
        LocalReply::Done(result) => result.expect("agent reply"),
        LocalReply::Pending(_) => panic!("inspection must be synchronous"),
    }
}

#[test]
fn plain_node_report_samples_cooked_clip_without_a_skeleton() {
    let mut clip = AnimationClip::empty("door", 1.0);
    clip.node_animation = Some(
        NodeAnimation::from_payload(include_bytes!("fixtures/format-corpus/node-tracks/v1.bin"))
            .expect("node fixture"),
    );
    let payload = clip
        .to_compressed_cooked_payload(None, &Default::default())
        .expect("cook");
    let decoded = AnimationClip::from_cooked_payload(&payload).expect("load");
    let mut app = App::new();
    let entity = app
        .world_mut()
        .spawn(Animator::single(decoded, false))
        .id()
        .to_bits();
    let report = call(
        &mut app,
        "anim.clip_report",
        json!({"entity":entity,"compression":false}),
    );
    assert!(report["skeleton"].is_null());
    let nodes = &report["clips"][0]["node_animation"];
    assert_eq!(nodes["nodes"][0]["index"], 7);
    assert_eq!(nodes["nodes"][0]["name"], "door");
    assert_eq!(nodes["nodes"][0]["is_joint"], false);
    assert!(nodes["validation_error"].is_null());
    for (i, x) in [0.0, 0.5, 1.0].into_iter().enumerate() {
        assert!(nodes["samples"][i]["error"].is_null());
        assert_eq!(nodes["samples"][i]["nodes"][0]["world"][12], x);
        assert_eq!(nodes["samples"][i]["nodes"][0]["delta"][12], x);
    }
    let state = call(&mut app, "anim.get_state", json!({"entity":entity}));
    assert_eq!(state["clip"]["node_tracks"], 1);
}
