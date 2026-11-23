// SPDX-License-Identifier: MPL-2.0
//! P-08: an actual saved hang snapshot is readable through the agent dispatcher.
#![cfg(all(target_os = "linux", target_env = "gnu"))]

use regia_agent::{LocalReply, call_local};
use regia_ecs::prelude::World;
use regia_platform::{crash, stack_sampling};
use serde_json::{Value, json};

fn call(world: &mut World, method: &str, params: Value) -> Value {
    match call_local(world, method, params, Some("hang-report-test")) {
        LocalReply::Done(result) => result.expect("test fixture"),
        LocalReply::Pending(_) => panic!("unexpected deferred {method}"),
    }
}

#[test]
fn saved_hang_report_is_listed_and_read_through_the_agent() {
    let dir = regia_test_utils::TempDir::new("agent-hang");
    crash::configure(
        Some(dir.path().to_owned()),
        crash::BuildInfo::detect(),
        regia_platform::OsInfo::detect(),
    );
    stack_sampling::install().expect("test fixture");
    let dump = stack_sampling::capture(&stack_sampling::Settings::default()).expect("test fixture");
    assert!(dump.threads.iter().any(|thread| thread.is_caller));
    let path =
        crash::write_hang_report("worker stalled token=private-value", &dump.text()).expect("test fixture");
    let name = path
        .file_name()
        .expect("test fixture")
        .to_str()
        .expect("test fixture");
    let mut world = World::new();
    let listed = call(&mut world, "diagnostics.crash_reports", json!({}));
    assert_eq!(listed["count"], 1);
    assert_eq!(listed["reports"][0]["name"], name);
    let read = call(&mut world, "diagnostics.crash_reports", json!({"name":name}));
    let text = read["content"].as_str().expect("test fixture");
    assert!(text.contains("hang: worker stalled"), "{text}");
    assert!(text.contains("thread stacks:"), "{text}");
    assert!(text.contains("[caller]"), "{text}");
    assert!(!text.contains("private-value"), "secret was not scrubbed");
    for thread in dump.threads {
        assert!(text.contains(&format!("thread {} ", thread.id)));
    }
    crash::configure(None, crash::BuildInfo::detect(), regia_platform::OsInfo::detect());
}
