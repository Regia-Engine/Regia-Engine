// SPDX-License-Identifier: MPL-2.0
//! CT-02 source-lint discovery through the real agent dispatcher (ADR-8248).

use regia_agent::protocol::RpcError;
use regia_agent::{AgentConfig, AgentPermissions, LocalReply, call_local};
use regia_app::Time;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
    match call_local(world, method, params, Some("content-lint-test")) {
        LocalReply::Done(result) => result,
        LocalReply::Pending(_) => panic!("{method} is synchronous"),
    }
}

fn world(root: &std::path::Path) -> World {
    let mut world = World::new();
    world.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.to_path_buf()),
    });
    world.insert_resource(AgentPermissions {
        config: AgentConfig {
            read_only: true,
            permissions: ["read".to_string()].into_iter().collect(),
            ..Default::default()
        },
        source: None,
    });
    regia_editor::agent_ext::install(&mut world);
    world
}

#[test]
fn dispatcher_lists_rules_and_resolves_policy_read_only() {
    let dir = regia_test_utils::unique_temp_dir("content-lint-agent");
    let policy = r#"(
        format_version: 1,
        profiles: { "mobile": (materials: Some((default_roughness: Some(0.4)))) },
        assets: [(glob: "assets/*.gltf", materials: Some((default_metallic: Some(0.2))))],
    )"#;
    std::fs::create_dir_all(dir.join("config")).expect("config dir");
    std::fs::write(dir.join("config/source_lint.ron"), policy).expect("policy");
    let mut world = world(&dir);
    assert!(regia_agent::guide_mentions("asset.content_lint_rules"));
    assert!(regia_agent::guide_mentions("asset.content_lint_policy"));

    let extension = world.resource::<regia_agent::extension::AgentExtension>();
    for name in ["asset.content_lint_rules", "asset.content_lint_policy"] {
        let schema = extension.method_schema(name).expect(name);
        assert!(!schema.mutating);
        assert_eq!(schema.permission, regia_agent::schema::Permission::Read);
    }

    let rules = call(&mut world, "asset.content_lint_rules", json!({})).expect("rules");
    assert_eq!(rules["ok"], true);
    let catalog = rules["rules"].as_array().expect("catalog");
    assert_eq!(catalog.len(), regia_asset_pipeline::source_lint::rules().len());
    // 17 base rules + 8 material-coverage rules + 5 geometry-complete rules (ADR-8347)
    // + 5 texture-lint rules (ADR-8536).
    assert_eq!(catalog.len(), 35);
    assert!(catalog.iter().any(|row| row["id"] == "texture_wrong_compression"));
    let ids: Vec<&str> = catalog.iter().filter_map(|rule| rule["id"].as_str()).collect();
    for id in [
        "emissive_strength",
        "extensions_used",
        "factor_missing",
        "factors_in_range",
        "orm_factors",
        "transmission_ior",
        "attenuation_distance",
        "extension_dependency",
        "extension_nesting",
        "ior_domain",
        "texture_color_space",
        "texture_index",
        "uv_channel",
        "volume_thickness",
        "gltf_coincident_advisory",
        "gltf_coincident_equivalent",
        "gltf_mesh_degenerate",
        "gltf_mesh_nonfinite",
        "gltf_mesh_zero_area",
    ] {
        assert!(ids.contains(&id), "{id}");
    }
    let importer = rules["importer"].as_array().expect("importer");
    assert!(
        importer
            .iter()
            .any(|row| row["name"] == "KHR_materials_volume" && row["status"] == "cooked")
    );
    assert!(
        importer
            .iter()
            .any(|row| row["name"] == "KHR_texture_transform" && row["status"] == "side_table")
    );
    let extra =
        call(&mut world, "asset.content_lint_rules", json!({"path": "a.gltf"})).expect_err("no params");
    assert_eq!(extra.data.expect("data")["reason"], "content_lint_params");

    let effective = call(
        &mut world,
        "asset.content_lint_policy",
        json!({"path": "assets/hero.gltf", "target": "mobile"}),
    )
    .expect("policy");
    assert_eq!(effective["ok"], true);
    assert_eq!(effective["target"], "mobile");
    assert_eq!(effective["path"], "assets/hero.gltf");
    assert_eq!(effective["materials"]["default_roughness"], 0.4);
    assert_eq!(effective["materials"]["default_metallic"], 0.2);
    assert_eq!(
        effective["precedence"],
        json!([
            "engine_defaults",
            "project_defaults",
            "target_profile",
            "asset_overlay"
        ])
    );
    assert!(
        !dir.join("assets/hero.gltf").exists(),
        "preview does not require the file"
    );

    let unknown = call(
        &mut world,
        "asset.content_lint_policy",
        json!({"target": "phone"}),
    )
    .expect_err("target");
    assert_eq!(unknown.data.expect("data")["reason"], "content_lint_target");
    let empty = call(&mut world, "asset.content_lint_policy", json!({"target": ""})).expect_err("empty");
    assert_eq!(empty.data.expect("data")["reason"], "content_lint_target");
    let escape = call(
        &mut world,
        "asset.content_lint_policy",
        json!({"path": "../outside.gltf"}),
    )
    .expect_err("escape");
    assert_eq!(escape.data.expect("data")["reason"], "path_outside_project");

    world.insert_resource(Time::default());
    let expect = call(
        &mut world,
        "play.expect",
        json!({
            "assertions": [{
                "name": "gameplay is not paused",
                "condition": {"resource": "Time", "path": "paused", "op": "eq", "value": false}
            }]
        }),
    )
    .expect("play.expect");
    assert_eq!(expect["pass"], true);
    assert_eq!(expect["results"][0]["pass"], true);
    std::fs::remove_dir_all(dir).expect("cleanup");
}
