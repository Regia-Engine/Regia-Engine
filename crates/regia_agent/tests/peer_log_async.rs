// SPDX-License-Identifier: MPL-2.0
//! ADR-8493: async peer-writer counters through logs.read, wait_until and expect.
use crossbeam_channel::unbounded;
use regia_agent::protocol::Request;
use regia_agent::server::Inbound;
use regia_agent::{AgentInbox, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_core::log::session::{AsyncWriterBounds, PeerLog, SessionOptions};
use regia_test_utils::TempDir;
use serde_json::{Value, json};

#[test]
fn two_peers_publish_async_counters_through_logs_wait_and_expect() {
    let project = TempDir::new("agent-peer-async");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(AgentSettings {
            project_dir: Some(project.path().to_owned()),
        })
        .add_plugins(AgentPlugin);
    app.finish_plugins();
    let (send, recv) = unbounded();
    app.insert_resource(AgentInbox(recv));
    app.update();
    let (reply, replies) = unbounded();
    let mut call = |method: &str, params: Value| {
        send.send(Inbound {
            request: Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: method.into(),
                params,
            },
            reply: reply.clone(),
        })
        .expect("test operation");
        for _ in 0..3 {
            app.update();
        }
        let answer = replies
            .try_recv()
            .expect("dispatcher answered within fixed steps");
        assert!(answer.error.is_none(), "{method}: {:?}", answer.error);
        answer.result.expect("test operation")
    };
    let guide = call("agent.guide", json!({ "topic": "log-extras" }));
    let guide_text = guide["text"].as_str().expect("guide text");
    assert!(guide_text.contains("with_async_writer"), "{guide_text}");
    assert!(guide_text.contains("peer_async"), "{guide_text}");
    assert!(guide_text.contains("incomplete=1"), "{guide_text}");

    let options = SessionOptions::default().with_async_writer(AsyncWriterBounds {
        max_records: 32,
        max_queued_bytes: 64 * 1024,
        max_message_bytes: 160,
    });
    let logs = project.join("peer-logs");
    let left = PeerLog::create(&logs, "async-001", 41, options).expect("peer 41");
    let right = PeerLog::create(&logs, "async-001", 42, options).expect("peer 42");
    left.line_at(
        3,
        tracing::Level::INFO,
        "net",
        format_args!("{}", "x".repeat(400)),
    )
    .expect("oversize line is dropped");
    left.hash_trace("world", 3, 0x41).expect("hash 41");
    right
        .line_at(4, tracing::Level::INFO, "net", format_args!("beta"))
        .expect("line 42");
    right.hash_trace("world", 4, 0x42).expect("hash 42");
    left.finish().expect("finish 41");
    right.finish().expect("finish 42");

    let left_hash = "t=3 peer=41 hash_trace name=world hash=0000000000000041";
    let right_hash = "t=4 peer=42 hash_trace name=world hash=0000000000000042";
    let waited = call(
        "play.wait_until",
        json!({ "condition": { "logs_contain": "peer=41 peer_async" } }),
    );
    assert_eq!(waited["satisfied"], true, "{waited}");
    let counters = call(
        "logs.read",
        json!({ "target": "regia_core::log::peer_async", "contains": "peer=41 ", "limit": 20 }),
    );
    let messages: Vec<_> = counters["records"]
        .as_array()
        .expect("counter records")
        .iter()
        .map(|record| record["message"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("dropped_lines=1") && message.contains("complete=1")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|message| message.contains("incomplete=0")),
        "{messages:?}"
    );
    let expected = call(
        "play.expect",
        json!({ "assertions": [
            { "name": "peer 41 checkpoint", "condition": { "logs_contain": left_hash } },
            { "name": "peer 42 checkpoint", "condition": { "logs_contain": right_hash } },
            { "name": "peer 42 clean", "condition": { "logs_contain": "peer=42 peer_async" } }
        ] }),
    );
    assert_eq!(expected["pass"], true, "{expected}");
    let left_bytes = std::fs::read_to_string(left.directory().join("world.hash")).expect("left hash");
    let right_bytes = std::fs::read_to_string(right.directory().join("world.hash")).expect("right hash");
    assert_eq!(left_bytes, "3 0000000000000041\n");
    assert_eq!(right_bytes, "4 0000000000000042\n");
}
