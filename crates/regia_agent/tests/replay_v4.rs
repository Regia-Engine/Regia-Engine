// SPDX-License-Identifier: MPL-2.0
//! V4 replay compatibility through the real agent dispatcher, with a deterministic frame clock.

use std::time::Duration;

use crossbeam_channel::unbounded;
use regia_agent::protocol::Request;
use regia_agent::server::Inbound;
use regia_agent::{AgentConfig, AgentInbox, AgentPermissions, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_test_utils::{unique_temp_dir, write_atomic};
use serde_json::{Value, json};

#[test]
fn v4_load_seek_step_and_failed_reload_through_dispatch() {
    let project = unique_temp_dir("replay-v4-dispatch");
    let bytes = include_bytes!("../../regia_framework/tests/fixtures/replay/v4_extensions.do27replay");
    write_atomic(&project.join("match.do27replay"), bytes).expect("fixture");
    let mut malformed = bytes.to_vec();
    malformed.push(0x7f);
    write_atomic(&project.join("bad.do27replay"), &malformed).expect("malformed fixture");
    let mut future = bytes.to_vec();
    future[4..8].copy_from_slice(&5_u32.to_le_bytes());
    write_atomic(&project.join("future.do27replay"), &future).expect("future fixture");

    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(AgentSettings {
            project_dir: Some(project.clone()),
        })
        .add_plugins(AgentPlugin)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.finish_plugins();
    let (inbox, requests) = unbounded();
    let (reply, responses) = unbounded();
    app.insert_resource(AgentInbox(requests));
    app.insert_resource(AgentPermissions {
        config: AgentConfig {
            rate_limit_per_second: u32::MAX,
            ..AgentConfig::default()
        },
        source: None,
    });
    let mut call = |method: &str, params: Value| {
        inbox
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply: reply.clone(),
            })
            .expect("request");
        app.update();
        app.update();
        responses.try_recv().expect("dispatcher responded")
    };
    let response = call("replay.load", json!({ "path": "match.do27replay" }));
    assert!(response.error.is_none(), "{response:?}");
    let loaded = response.result.expect("loaded");
    assert_eq!(loaded["header"]["version"], 4);
    assert_eq!(loaded["header"]["players"][1]["callsign"], "Ödön");
    assert_eq!(loaded["end_tick"], 2);
    assert_eq!(loaded["counts"]["fulls"], 2);
    assert_eq!(loaded["verify"]["hash_checked"], 0);
    assert_eq!(loaded["verify"]["hash_unchecked"], 2);

    let stepped = call("replay.step", json!({ "ticks": 1 })).result.expect("step");
    assert_eq!(stepped["position"]["tick"], 1);
    assert_eq!(stepped["events_total"], 4);
    let seek = call("replay.seek", json!({ "tick": 2 })).result.expect("seek");
    assert_eq!(seek["position"]["tick"], 2);
    let at_end = call("replay.step", json!({ "ticks": 1 }))
        .result
        .expect("step at end");
    assert_eq!(at_end["advanced"], 0);
    assert_eq!(at_end["events_total"], 0);

    for (path, code) in [
        ("bad.do27replay", "unknown_tag"),
        ("future.do27replay", "unsupported_version"),
    ] {
        let error = call("replay.load", json!({ "path": path }))
            .error
            .expect("refused");
        assert_eq!(error.code, regia_agent::protocol::INVALID_PARAMS);
        assert!(error.message.contains(&format!("[{code}]")), "{error:?}");
        let current = call("replay.step", json!({ "ticks": 0 }))
            .result
            .expect("old player survives");
        assert_eq!(current["position"]["tick"], 2);
        assert_eq!(current["position"]["end_tick"], 2);
    }
    drop(app);
    std::fs::remove_dir_all(project).expect("cleanup");
}
