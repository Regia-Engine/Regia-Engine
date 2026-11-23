// SPDX-License-Identifier: MPL-2.0
//! Spec 54 test 18 (agent): `render.breadcrumbs` with the writer off answers
//! `F0003`/`breadcrumbs_off`, and `source: "last_bundle"` reads the report back from the newest
//! device-lost crash bundle.
//!
//! Its own test binary: the breadcrumb writer and the crash folder are process-wide, so one
//! test runs both steps in order.

use std::time::Duration;

use regia_agent::breadcrumbs::install_breadcrumb_methods;
use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_framework::feedback::crash::write_device_lost_bundle;
use regia_framework::feedback::{FeedbackConfig, FeedbackPlugin};
use regia_framework::settings::GameSettings;
use regia_render::breadcrumbs::{self, BreadcrumbConfig, CrumbList, SubmissionId};
use regia_render::gpu_health::DeviceLostInfo;
use serde_json::{Value, json};

fn call(world: &mut World, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let handler = world
        .resource::<AgentExtension>()
        .method("render.breadcrumbs")
        .map(|(_, h)| h)
        .expect("registered");
    handler(world, &params)
}

fn submit_frame(frame: u64, passes: &[&'static str]) -> SubmissionId {
    let mut list = CrumbList::new();
    for name in passes {
        drop(breadcrumbs::global().pass(&mut list, name));
    }
    breadcrumbs::note_submit(&[&list], frame)
}

#[test]
fn breadcrumbs_off_and_last_bundle() {
    // 1. Writer off: the live source is refused with a reason, not an empty report.
    breadcrumbs::configure(BreadcrumbConfig {
        enabled: false,
        ..BreadcrumbConfig::default()
    });
    let mut methods = World::new();
    install_breadcrumb_methods(&mut methods);
    let err = call(&mut methods, json!({})).expect_err("writer off");
    let data = err.data.expect("data");
    assert_eq!(
        (data["code"].clone(), data["reason"].clone()),
        (json!("F0003"), json!("breadcrumbs_off"))
    );
    breadcrumbs::configure(BreadcrumbConfig::default());
    call(&mut methods, json!({})).expect("on again");

    // 2. A device-lost bundle in a temp feedback folder; `last_bundle` parses its report.
    let dir = regia_test_utils::unique_temp_dir("agent-breadcrumbs-bundle");
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.add_plugins(FeedbackPlugin {
        config: FeedbackConfig {
            dir: Some(dir.clone()),
            ..FeedbackConfig::default()
        },
    });
    app.insert_resource(GameSettings::default());
    for _ in 0..5 {
        app.update();
    }
    let first = submit_frame(1, &["shadow_cascades", "opaque_forward"]);
    submit_frame(2, &["shadow_cascades", "opaque_forward"]);
    breadcrumbs::note_completed(first);
    let info = DeviceLostInfo {
        reason: "unknown",
        message: "scripted loss for the agent".to_owned(),
    };
    write_device_lost_bundle(&info).expect("a bundle");

    let r = call(&mut methods, json!({ "source": "last_bundle", "frames": 1 })).expect("bundle read");
    assert_eq!(r["source"], "last_bundle");
    assert_eq!(r["report"]["format_version"], 1);
    assert_eq!(r["report"]["lost"]["reason"], "unknown");
    assert_eq!(r["report"]["lost"]["message"], "scripted loss for the agent");
    assert_eq!(r["report"]["level"], "fenced");
    assert_eq!(r["level_number"], 2);
    assert_eq!(r["report"]["counters"]["submitted"], 2);
    assert_eq!(r["report"]["counters"]["completed"], 1);
    let suspects: Vec<&str> = r["report"]["suspects"]
        .as_array()
        .expect("suspects")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert_eq!(
        suspects,
        ["shadow_cascades", "opaque_forward"],
        "submission 2's passes"
    );
    assert_eq!(
        r["report"]["frames"].as_array().map(Vec::len),
        Some(1),
        "`frames` caps the bundle's frames too"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
