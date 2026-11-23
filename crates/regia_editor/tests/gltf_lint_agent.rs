// SPDX-License-Identifier: MPL-2.0
//! C-06 lint through the real agent dispatcher, without a window or GPU.

#[path = "../../regia_asset_pipeline/tests/support/gltf_lint.rs"]
mod fixture;

use regia_agent::protocol::RpcError;
use regia_agent::{AgentConfig, AgentPermissions, LocalReply, call_local};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

fn call(world: &mut World, params: Value) -> Result<Value, RpcError> {
    match call_local(world, "asset.gltf_lint", params, Some("lint-test")) {
        LocalReply::Done(result) => result,
        LocalReply::Pending(_) => panic!("lint is synchronous"),
    }
}

fn world(root: &std::path::Path) -> World {
    let mut world = World::new();
    world.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.to_path_buf()),
    });
    regia_editor::agent_ext::install(&mut world);
    world
}

#[test]
fn dispatcher_reports_geometry_read_only_and_enforces_read_permission() {
    let dir = regia_test_utils::unique_temp_dir("gltf-lint-agent");
    let bytes = fixture::Fixture::stretched().glb();
    std::fs::write(dir.join("model.glb"), &bytes).expect("fixture");
    let mut world = world(&dir);
    assert!(regia_agent::guide_mentions("asset.gltf_lint"));
    let extension = world.resource::<regia_agent::extension::AgentExtension>();
    let schema = extension.method_schema("asset.gltf_lint").expect("schema");
    assert!(!schema.mutating);
    assert_eq!(schema.permission, regia_agent::schema::Permission::Read);
    world.insert_resource(AgentPermissions {
        config: AgentConfig {
            read_only: true,
            permissions: ["read".to_string()].into_iter().collect(),
            ..Default::default()
        },
        source: None,
    });
    let result = call(&mut world, json!({"path": "model.glb"})).expect("read-only works");
    assert_eq!(result["ok"], true);
    assert_eq!(result["triangles"], 2);
    assert_eq!(
        result["coincident_triangles"][0]["pair"],
        json!({"keeper": 0, "suspect": 1})
    );
    assert_eq!(std::fs::read(dir.join("model.glb")).expect("source"), bytes);
    world
        .resource_mut::<AgentPermissions>()
        .config
        .permissions
        .remove("read");
    let denied = call(&mut world, json!({"path": "model.glb"})).expect_err("read permission required");
    assert_eq!(denied.code, regia_agent::protocol::PERMISSION_DENIED);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn dispatcher_checks_settings_budgets_and_paths() {
    let dir = regia_test_utils::unique_temp_dir("gltf-lint-agent-errors");
    std::fs::write(dir.join("model.glb"), fixture::Fixture::stretched().glb()).expect("fixture");
    let mut world = world(&dir);
    for (settings, reason) in [
        (json!({"geometry": {"separation": 0}}), "gltf_lint_settings"),
        (json!({"typo": 1}), "gltf_lint_settings"),
        (json!({"max_triangles": 1}), "gltf_lint_limit"),
    ] {
        let error =
            call(&mut world, json!({"path": "model.glb", "settings": settings})).expect_err("refused");
        assert_eq!(error.data.expect("data")["reason"], reason);
    }
    let result = call(
        &mut world,
        json!({"path": "model.glb", "settings": {"geometry": {"anisotropy_ratio": 200.0}}}),
    )
    .expect("custom policy");
    assert_eq!(result["coincident_triangles"], json!([]));
    let escape = call(&mut world, json!({"path": "../model.glb"})).expect_err("project boundary");
    assert_eq!(escape.data.expect("data")["reason"], "path_outside_project");
    let missing_path = call(&mut world, json!({})).expect_err("path required");
    assert_eq!(missing_path.code, regia_agent::protocol::INVALID_PARAMS);
    let absent = call(&mut world, json!({"path": "absent.glb"})).expect_err("file required");
    assert_eq!(absent.data.expect("data")["code"], "F0003");
    std::fs::remove_dir_all(dir).expect("cleanup");
}

mod robust_workflow {
    //! CT-05 robust mode through the dispatcher, checked with wait_until and expect (ADR-8347).
    use regia_agent::{LocalReply, call_local};
    use regia_app::prelude::*;
    use serde_json::{Value, json};

    #[derive(Component, Reflect, Clone, Debug, Default)]
    #[reflect(Component, Default)]
    struct RobustLintEvidence {
        tolerance_rows: u64,
        safe_delete_rows: u64,
        candidate_pairs: u64,
    }

    fn call(app: &mut App, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
        match call_local(app.world_mut(), method, params, Some("robust-lint-test")) {
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
    fn robust_mode_rows_are_waited_for_and_expected() {
        let dir = regia_test_utils::unique_temp_dir("gltf-lint-agent-robust");
        let bytes = super::fixture::Fixture::stretched().glb();
        regia_test_utils::write_atomic(&dir.join("model.glb"), &bytes).expect("fixture");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(dir.clone()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.register_type::<RobustLintEvidence>();
        regia_editor::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let legacy = call(&mut app, "asset.gltf_lint", json!({"path": "model.glb"})).expect("legacy");
        assert_eq!(legacy["mode"], "legacy");
        assert_eq!(legacy["robust_coincidences"], json!([]));
        let report = call(
            &mut app,
            "asset.gltf_lint",
            json!({"path": "model.glb", "settings": {"mode": "robust", "robust": {"tolerance_meters": 0.0001}}}),
        )
        .expect("robust");
        assert_eq!(report["mode"], "robust");
        assert_eq!(report["coincident_triangles"], json!([]));
        let rows = report["robust_coincidences"].as_array().expect("rows");
        let entity = app
            .world_mut()
            .spawn(RobustLintEvidence {
                tolerance_rows: rows.iter().filter(|r| r["agreement"] == "tolerance").count() as u64,
                safe_delete_rows: rows.iter().filter(|r| r["safe_delete_candidate"] == true).count() as u64,
                candidate_pairs: report["robust_work"]["candidate_pairs"]
                    .as_u64()
                    .expect("counter"),
            })
            .id();
        let selector = json!(entity.to_bits());
        let condition = json!({"all": [
            {"entity": selector, "component": "RobustLintEvidence", "path": "tolerance_rows", "op": "eq", "value": 1},
            {"entity": selector, "component": "RobustLintEvidence", "path": "safe_delete_rows", "op": "eq", "value": 0},
            {"entity": selector, "component": "RobustLintEvidence", "path": "candidate_pairs", "op": "eq", "value": 1}
        ]});
        let waited = call(&mut app, "play.wait_until", json!({"condition": condition})).expect("wait_until");
        assert_eq!(waited["satisfied"], true, "{waited}");
        let checked = call(
            &mut app,
            "play.expect",
            json!({"assertions": [{"name": "robust lint evidence", "condition": condition}]}),
        )
        .expect("expect");
        assert_eq!(checked["pass"], true, "{checked}");
        let budget = call(
            &mut app,
            "asset.gltf_lint",
            json!({"path": "model.glb", "settings": {"mode": "robust", "robust": {"max_candidate_pairs": 0}}}),
        )
        .expect_err("zero budget");
        assert_eq!(budget.data.expect("data")["reason"], "gltf_lint_settings");
        assert_eq!(std::fs::read(dir.join("model.glb")).expect("source"), bytes);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
}
