// SPDX-License-Identifier: MPL-2.0
//! NL-07: peer checkpoints and recovered sessions through the real agent dispatcher.
use crossbeam_channel::unbounded;
use regia_agent::protocol::Request;
use regia_agent::server::Inbound;
use regia_agent::{AgentInbox, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_core::log::session::{PeerLog, SessionOptions};
use regia_test_utils::TempDir;
use serde_json::{Value, json};

#[test]
fn peer_hashes_and_unclean_sessions_are_visible_to_logs_wait_and_expect() {
    let project = TempDir::new("agent-peer-logs");
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
    let options = SessionOptions::default();
    let guide = call("agent.guide", json!({ "topic": "log-extras" }));
    let guide_text = guide["text"].as_str().expect("test operation");
    assert!(guide_text.contains("hash_trace"));
    assert!(guide_text.contains("policy_version"));
    assert!(guide_text.contains("sanitized=1"));
    let logs = project.join("peer-logs");
    let old = PeerLog::create(&logs, "001", 8066, options).expect("test operation");
    let session = call(
        "logs.read",
        json!({ "target": "regia_core::log::session", "contains": "peer=8066 " }),
    );
    assert_eq!(session["records"].as_array().expect("session records").len(), 1);
    assert!(
        session["records"][0]["message"]
            .as_str()
            .expect("session message")
            .contains(&format!("dev_env_enabled={}", regia_core::log::devenv::ENABLED))
    );
    old.set_tick(37);
    old.hash_trace("world", 37, 0x1234).expect("test operation");
    old.line(tracing::Level::WARN, "net", format_args!("peer diagnostic"))
        .expect("test operation");
    old.line(
        tracing::Level::WARN,
        "net",
        format_args!("token=agent-secret-value ann@example.com"),
    )
    .expect("test operation");
    old.flush().expect("test operation");
    let checkpoint = "t=37 peer=8066 hash_trace name=world hash=0000000000001234";
    let read = call(
        "logs.read",
        json!({ "target": "regia_core::log::hash_trace", "contains": "peer=8066 " }),
    );
    assert_eq!(read["records"].as_array().expect("test operation").len(), 1);
    assert_eq!(read["records"][0]["message"], checkpoint);
    let waited = call(
        "play.wait_until",
        json!({ "condition": { "logs_contain": checkpoint } }),
    );
    assert_eq!(waited["satisfied"], true, "{waited}");
    drop(old);
    let next = PeerLog::create(&logs, "002", 8066, options).expect("test operation");
    let found = next.recovered().expect("unclean evidence");
    assert!(found.sanitized);
    assert_eq!(
        found.policy_version,
        regia_core::log::session::RECOVERY_PRIVACY_POLICY_VERSION
    );
    let unclean = call(
        "logs.read",
        json!({ "target": "regia_core::log::unclean", "contains": "sanitized=1" }),
    );
    let notice = unclean["records"][0]["message"].as_str().expect("unclean notice");
    assert!(notice.contains("policy_version=1"), "{notice}");
    assert!(notice.contains("sanitized=1 complete=1"), "{notice}");
    assert!(!notice.contains("agent-secret-value"), "{notice}");
    assert!(!notice.contains("ann@example.com"), "{notice}");
    let tail = std::fs::read(found.directory.join("lastlines.log")).expect("published tail");
    assert!(
        !tail
            .windows(b"agent-secret-value".len())
            .any(|w| w == b"agent-secret-value")
    );
    assert!(
        !tail
            .windows(b"ann@example.com".len())
            .any(|w| w == b"ann@example.com")
    );
    assert!(tail.windows(b"[redacted]".len()).any(|w| w == b"[redacted]"));
    assert!(tail.windows(b"[email]".len()).any(|w| w == b"[email]"));
    let privacy_wait = call(
        "play.wait_until",
        json!({ "condition": { "logs_contain": "sanitized=1 complete=1 policy_version=1" } }),
    );
    assert_eq!(privacy_wait["satisfied"], true, "{privacy_wait}");
    let expected = call(
        "play.expect",
        json!({ "assertions": [
        { "name": "checkpoint", "condition": { "logs_contain": checkpoint } },
        { "name": "unclean evidence", "condition": { "logs_contain": "unclean_session session=001-peer8066" } },
        { "name": "sanitized report", "condition": { "logs_contain": "sanitized=1 complete=1 policy_version=1" } }
    ] }),
    );
    assert_eq!(expected["pass"], true, "{expected}");
    next.finish().expect("test operation");
}
