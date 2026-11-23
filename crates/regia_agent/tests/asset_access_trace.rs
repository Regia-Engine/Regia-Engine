// SPDX-License-Identifier: MPL-2.0
//! The trace view is read-only, bounded, and available through the real dispatcher.

use regia_agent::{AgentConfig, AgentPermissions, LocalReply, call_local};
use regia_app::prelude::*;
use serde_json::{Value, json};

fn call(world: &mut World, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    match call_local(world, "assets.access_trace", params, None) {
        LocalReply::Done(result) => result,
        LocalReply::Pending(_) => panic!("trace query must be immediate"),
    }
}

#[test]
fn agent_reads_real_missed_open_and_validates_limit() {
    let root = regia_test_utils::unique_temp_dir("agent-access-trace");
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "agent_trace_probe", "--ignored", "--nocapture"])
        .env("REGIA_ASSET_TRACE", root.join("trace.log"))
        .env("REGIA_TRACE_TEST_ROOT", &root)
        .output()
        .expect("child starts");
    assert!(output.status.success(), "{output:?}");
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
#[ignore = "isolated process fixture, invoked by the parent"]
fn agent_trace_probe() {
    let root = std::env::var_os("REGIA_TRACE_TEST_ROOT").expect("root");
    let mut server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
        project_root: root.into(),
        platform: "linux".into(),
        content_dir: None,
    })
    .expect("server");
    assert!(
        server
            .read_cooked(regia_asset::AssetId(regia_core::id::Guid(42)))
            .is_err()
    );
    let mut world = World::new();
    world.insert_resource(AgentPermissions {
        config: AgentConfig {
            read_only: true,
            ..Default::default()
        },
        source: None,
    });
    let result = call(&mut world, json!({"limit": 1})).expect("read-only call");
    assert_eq!(result["enabled"], true);
    assert_eq!(result["events"][0]["outcome"], "miss");
    assert_eq!(result["events"][0]["operation"], "open");
    let total = result["total"].clone();
    let counters = call(&mut world, json!({"limit": 0})).expect("counters only");
    assert_eq!(counters["total"], total, "query must not trace itself");
    assert_eq!(counters["events"], json!([]));
    for limit in [json!(-1), json!(1.5), json!("all"), json!([])] {
        let error = call(&mut world, json!({"limit": limit})).expect_err("bad limit");
        assert_eq!(error.code, regia_agent::protocol::INVALID_PARAMS);
    }
    assert!(call(&mut world, json!({"limit": u64::MAX})).is_ok());
}
