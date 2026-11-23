// SPDX-License-Identifier: MPL-2.0
//! CT-01 workflow through the actual dispatcher and wait_until/expect predicates.
use regia_agent::{LocalReply, call_local};
use regia_app::prelude::*;
use serde_json::{Value, json};

#[derive(Component, Reflect, Clone, Debug, Default)]
#[reflect(Component, Default)]
struct LintEvidence {
    geometry_done: bool,
    metadata_buffer_bytes: u64,
    effective_json_limit: u64,
}

fn call(app: &mut App, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    match call_local(app.world_mut(), method, params, Some("content-budget-test")) {
        LocalReply::Done(result) => result,
        LocalReply::Pending(answer) => {
            app.update();
            let response = answer.try_recv().expect("one deterministic update");
            match (response.result, response.error) {
                (_, Some(error)) => Err(error),
                (Some(result), None) => Ok(result),
                _ => panic!("missing response"),
            }
        }
    }
}

#[test]
fn agent_budget_workflow_waits_for_and_asserts_report_evidence() {
    let dir = regia_test_utils::unique_temp_dir("content-budget-agent");
    regia_test_utils::write_atomic(
        &dir.join("model.glb"),
        &regia_asset_pipeline::fuzz_entry::glb_fixture(),
    )
    .expect("fixture");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(dir.clone()),
    });
    app.add_plugins(regia_agent::AgentPlugin);
    app.register_type::<LintEvidence>();
    regia_editor::agent_ext::install(app.world_mut());
    regia_editor::import_fuzz::install(app.world_mut());
    app.finish_plugins();
    app.update();
    assert!(regia_agent::guide_mentions("asset.gltf_material_lint"));
    let targets = call(&mut app, "fuzz.targets", json!({})).expect("fuzz targets");
    assert!(
        targets["targets"]
            .as_array()
            .expect("targets")
            .iter()
            .any(|target| target["name"] == "content.gltf_bounded")
    );
    let limits = json!({"max_json_bytes":4096});
    let geometry = call(
        &mut app,
        "asset.gltf_lint",
        json!({"path":"model.glb","settings":{"input_limits":limits}}),
    )
    .expect("geometry");
    let material = call(
        &mut app,
        "asset.gltf_material_lint",
        json!({"path":"model.glb","settings":{"input_limits":limits}}),
    )
    .expect("material");
    assert_eq!(geometry["input_limits"]["max_json_bytes"], 4096);
    let entity = app
        .world_mut()
        .spawn(LintEvidence {
            geometry_done: geometry["ok"].as_bool().expect("ok"),
            metadata_buffer_bytes: material["input_usage"]["buffer_bytes_allocated"]
                .as_u64()
                .expect("counter"),
            effective_json_limit: material["input_limits"]["max_json_bytes"]
                .as_u64()
                .expect("limit"),
        })
        .id();
    let selector = json!(entity.to_bits());
    let condition = json!({"all":[
        {"entity":selector,"component":"LintEvidence","path":"geometry_done","op":"eq","value":true},
        {"entity":selector,"component":"LintEvidence","path":"metadata_buffer_bytes","op":"eq","value":0},
        {"entity":selector,"component":"LintEvidence","path":"effective_json_limit","op":"eq","value":4096}
    ]});
    let waited = call(&mut app, "play.wait_until", json!({"condition":condition})).expect("wait_until");
    assert_eq!(waited["satisfied"], true, "{waited}");
    let checked = call(
        &mut app,
        "play.expect",
        json!({"assertions":[{"name":"bounded lint evidence","condition":condition}]}),
    )
    .expect("expect");
    assert_eq!(checked["pass"], true, "{checked}");
    for method in [
        "asset.gltf_lint",
        "asset.gltf_material_lint",
        "asset.gltf_material_fix",
    ] {
        let error = call(
            &mut app,
            method,
            json!({"path":"model.glb","settings":{"input_limits":{"max_json_bytes":1}}}),
        )
        .expect_err("limit");
        assert_eq!(error.data.expect("data")["reason"], "content_lint_limit");
    }
    std::fs::remove_dir_all(dir).expect("cleanup");
}
