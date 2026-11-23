// SPDX-License-Identifier: MPL-2.0
//! CT-04: source-lint jobs, Problems rows, and threshold provenance (ADR-8391).
//!
//! Synchronization is channels and counters. No sleeps and no elapsed-time assertions.

use std::path::Path;

use regia_agent::protocol::{self, RpcError};
use regia_agent::{AgentConfig, AgentPermissions, LocalReply, call_local};
use regia_app::prelude::*;
use regia_asset_pipeline::source_lint::{GeometryOverlay, MaterialOverlay, SourceLintPolicy};
use regia_asset_pipeline::validators::Severity;
use regia_editor::asset_panels::{AssetPanels, ProblemRow};
use regia_editor::content_lint_jobs::{self, LintDone, LintTestGate};
use serde_json::{Value, json};

const BARE: &[u8] = br#"{"asset":{"version":"2.0"}}"#;
const EMPTY_MATERIAL: &[u8] = br#"{"asset":{"version":"2.0"},"materials":[{}]}"#;

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
    match call_local(world, method, params, Some("content-lint-jobs")) {
        LocalReply::Done(result) => result,
        LocalReply::Pending(_) => panic!("{method} is synchronous"),
    }
}

fn open_world(root: &Path, read_only: bool) -> World {
    let mut world = World::new();
    world.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.to_path_buf()),
    });
    let config = if read_only {
        AgentConfig {
            read_only: true,
            permissions: ["read".to_string()].into_iter().collect(),
            ..Default::default()
        }
    } else {
        AgentConfig::default()
    };
    world.insert_resource(AgentPermissions { config, source: None });
    regia_editor::agent_ext::install(&mut world);
    world
}

fn write_model(dir: &Path, rel: &str, bytes: &[u8]) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent");
    }
    regia_test_utils::write_atomic(&path, bytes).expect("write");
}

fn source_codes(world: &World) -> Vec<String> {
    world
        .get_resource::<AssetPanels>()
        .map(|panels| {
            panels
                .problems
                .rows
                .iter()
                .filter(|row| row.validator == "source_lint")
                .map(|row| row.code.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn tick(world: &mut World) {
    regia_editor::editor_jobs::tick(world);
}

fn parked(gate: &LintTestGate) {
    gate.wait_parked().expect("worker parked");
}

fn release(gate: &LintTestGate) {
    gate.release_worker().expect("worker released");
}

fn done(gate: &LintTestGate) {
    gate.wait_done().expect("worker stored its outcome");
}

fn done_ping(ping: &LintDone) {
    ping.wait_done().expect("worker stored its outcome");
}

#[test]
fn paused_worker_leaves_editor_ticks_runnable() {
    let dir = regia_test_utils::unique_temp_dir("content-lint-jobs-pause");
    write_model(&dir, "assets/hero.gltf", EMPTY_MATERIAL);
    let mut world = open_world(&dir, false);
    let gate = content_lint_jobs::inject_pause(&mut world);
    let started = call(
        &mut world,
        "asset.content_lint_start",
        json!({"paths": ["assets/hero.gltf"]}),
    )
    .expect("start");
    assert_eq!(started["state"], "running");
    parked(&gate);
    for _ in 0..3 {
        tick(&mut world);
    }
    let status = call(&mut world, "asset.content_lint_status", json!({})).expect("status");
    assert_eq!(status["state"], "running");
    assert_eq!(content_lint_jobs::scans_started(&world), 1);
    release(&gate);
    parked(&gate);
    release(&gate);
    done(&gate);
    tick(&mut world);
    let status = call(&mut world, "asset.content_lint_status", json!({})).expect("done");
    assert_eq!(status["state"], "done");
    assert_eq!(status["findings_published"], true);
    assert!(
        source_codes(&world).iter().any(|code| code == "factor_missing"),
        "published rows: {:?}",
        source_codes(&world)
    );
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn cancel_finishes_with_content_lint_cancelled_and_publishes_nothing() {
    let dir = regia_test_utils::unique_temp_dir("content-lint-jobs-cancel");
    write_model(&dir, "assets/hero.gltf", EMPTY_MATERIAL);
    let before = std::fs::read(dir.join("assets/hero.gltf")).expect("bytes");
    let mut world = open_world(&dir, false);
    let gate = content_lint_jobs::inject_pause(&mut world);
    call(
        &mut world,
        "asset.content_lint_start",
        json!({"paths": ["assets/hero.gltf"]}),
    )
    .expect("start");
    parked(&gate);
    release(&gate);
    parked(&gate);
    let requested = call(&mut world, "asset.content_lint_cancel", json!({})).expect("cancel");
    assert_eq!(requested["requested"], true);
    release(&gate);
    done(&gate);
    tick(&mut world);
    let status = call(&mut world, "asset.content_lint_status", json!({})).expect("status");
    assert_eq!(status["state"], "cancelled");
    assert_eq!(status["code"], "content_lint_cancelled");
    assert_eq!(status["findings_published"], false);
    let read = call(&mut world, "asset.content_lint_read", json!({})).expect("read");
    assert_eq!(read["code"], "content_lint_cancelled");
    assert_eq!(read["findings"], json!([]));
    assert!(source_codes(&world).is_empty());
    tick(&mut world);
    assert!(source_codes(&world).is_empty());
    assert_eq!(
        std::fs::read(dir.join("assets/hero.gltf")).expect("bytes"),
        before
    );
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn source_change_mid_scan_discards_stale_findings() {
    let dir = regia_test_utils::unique_temp_dir("content-lint-jobs-stale");
    write_model(&dir, "assets/hero.gltf", EMPTY_MATERIAL);
    let mut world = open_world(&dir, false);
    let gate = content_lint_jobs::inject_pause(&mut world);
    call(
        &mut world,
        "asset.content_lint_start",
        json!({"paths": ["assets/hero.gltf"]}),
    )
    .expect("start");
    parked(&gate);
    release(&gate);
    parked(&gate);
    write_model(&dir, "assets/hero.gltf", BARE);
    content_lint_jobs::note_path(&mut world, "assets/hero.gltf");
    release(&gate);
    done(&gate);
    tick(&mut world);
    assert!(content_lint_jobs::discarded_scans(&world) >= 1);
    assert_eq!(content_lint_jobs::scans_started(&world), 2);
    assert!(
        !source_codes(&world).iter().any(|code| code == "factor_missing"),
        "stale rows: {:?}",
        source_codes(&world)
    );
    let read = call(
        &mut world,
        "asset.content_lint_read",
        json!({"path": "assets/hero.gltf"}),
    )
    .expect("read");
    assert_eq!(read["code"], "content_lint_stale");
    assert_eq!(read["findings"], json!([]));
    parked(&gate);
    release(&gate);
    parked(&gate);
    release(&gate);
    done(&gate);
    tick(&mut world);
    assert!(
        !source_codes(&world).iter().any(|code| code == "factor_missing"),
        "follow-up rows: {:?}",
        source_codes(&world)
    );
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn import_and_save_burst_starts_one_scan() {
    let dir = regia_test_utils::unique_temp_dir("content-lint-jobs-burst");
    write_model(&dir, "assets/a.gltf", BARE);
    write_model(&dir, "assets/b.glb", BARE);
    let mut world = open_world(&dir, false);
    let gate = content_lint_jobs::inject_pause(&mut world);
    for _ in 0..6 {
        content_lint_jobs::note_path(&mut world, "assets/a.gltf");
        content_lint_jobs::note_imported(
            &mut world,
            &[
                "assets/b.glb".to_string(),
                "assets/a.gltf".to_string(),
                "notes.txt".to_string(),
                "maps/town.level.ron".to_string(),
            ],
        );
        content_lint_jobs::note_path(&mut world, "/abs/skip.gltf");
        content_lint_jobs::note_path(&mut world, "C:/skip.gltf");
    }
    assert_eq!(content_lint_jobs::scans_started(&world), 0);
    assert!(content_lint_jobs::scan_queued(&world));
    tick(&mut world);
    assert_eq!(content_lint_jobs::scans_started(&world), 1);
    parked(&gate);
    content_lint_jobs::note_path(&mut world, "assets/a.gltf");
    content_lint_jobs::note_imported(&mut world, &["assets/b.glb".to_string()]);
    tick(&mut world);
    assert_eq!(content_lint_jobs::scans_started(&world), 1);
    assert!(content_lint_jobs::scan_queued(&world));
    call(&mut world, "asset.content_lint_cancel", json!({})).expect("cancel");
    release(&gate);
    done(&gate);
    content_lint_jobs::dismiss_pending(&mut world);
    tick(&mut world);
    assert_eq!(content_lint_jobs::scans_started(&world), 1);
    let status = call(&mut world, "asset.content_lint_status", json!({})).expect("status");
    assert_eq!(status["code"], "content_lint_cancelled");
    assert_eq!(status["findings_published"], false);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn human_and_rpc_resolve_the_same_policy_and_findings() {
    let dir = regia_test_utils::unique_temp_dir("content-lint-jobs-policy");
    write_model(&dir, "assets/hero.gltf", EMPTY_MATERIAL);
    let file_len = std::fs::metadata(dir.join("assets/hero.gltf"))
        .expect("len")
        .len();
    let mut world = open_world(&dir, false);
    let ping = content_lint_jobs::inject_done(&mut world);
    let geometry = GeometryOverlay {
        max_triangles: Some(1000),
        ..GeometryOverlay::default()
    };
    let materials = MaterialOverlay {
        default_roughness: Some(0.4),
        ..MaterialOverlay::default()
    };
    let human = content_lint_jobs::human_apply_thresholds(
        &mut world,
        "mobile",
        Some(geometry),
        Some(materials),
        Some("assets/hero.gltf"),
    )
    .expect("human thresholds");
    let rpc = call(
        &mut world,
        "asset.content_lint_policy",
        json!({"path": "assets/hero.gltf", "target": "mobile"}),
    )
    .expect("policy");
    assert_eq!(human["geometry"], rpc["geometry"]);
    assert_eq!(human["materials"], rpc["materials"]);
    assert_eq!(human["provenance"], rpc["provenance"]);
    assert_eq!(rpc["geometry"]["max_triangles"], 1000);
    let roughness = rpc["materials"]["default_roughness"].as_f64().expect("roughness");
    assert!((roughness - 0.4).abs() < 1e-9);
    assert_eq!(rpc["provenance"]["geometry"]["max_triangles"], "target_profile");
    assert_eq!(
        rpc["provenance"]["materials"]["default_roughness"],
        "target_profile"
    );
    assert_eq!(rpc["provenance"]["materials"]["profile"], "engine_defaults");
    assert_eq!(rpc["provenance"]["geometry"]["quantization"], "engine_defaults");
    let again = call(
        &mut world,
        "asset.content_lint_thresholds",
        json!({
            "target": "mobile",
            "path": "assets/hero.gltf",
            "geometry": {"max_triangles": 1000},
            "materials": {"default_roughness": 0.4}
        }),
    )
    .expect("rpc thresholds");
    assert_eq!(
        again["geometry"]["max_triangles"],
        rpc["geometry"]["max_triangles"]
    );
    assert_eq!(
        again["materials"]["default_roughness"],
        rpc["materials"]["default_roughness"]
    );
    assert_eq!(again["provenance"], rpc["provenance"]);
    let loaded = SourceLintPolicy::load_project(&dir).expect("load");
    let resolved = loaded
        .resolve("mobile", Some("assets/hero.gltf"))
        .expect("resolve");
    assert_eq!(resolved.geometry.max_triangles, 1000);
    assert!((resolved.materials.default_roughness - 0.4).abs() < 1e-9);

    call(
        &mut world,
        "asset.content_lint_start",
        json!({"paths": ["assets/hero.gltf"], "target": "mobile"}),
    )
    .expect("start");
    done_ping(&ping);
    tick(&mut world);
    let viewed = content_lint_jobs::human_view(&mut world, "mobile", Some("assets/hero.gltf")).expect("view");
    let read = call(
        &mut world,
        "asset.content_lint_read",
        json!({"target": "mobile", "path": "assets/hero.gltf"}),
    )
    .expect("read");
    assert_eq!(viewed["findings"], read["findings"]);
    let findings = read["findings"].as_array().expect("findings");
    assert!(findings.iter().any(|row| row["rule_id"] == "factor_missing"));
    assert_eq!(read["work"]["sources_processed"], 1);
    assert_eq!(read["work"]["bytes_hashed"], file_len);
    assert!(read["work"]["findings"].as_u64().expect("findings") >= 1);
    // One source runs every rule of the catalog once. The catalog grows as lint lanes add
    // rules (22 at CT-04, 35 after the material-coverage, geometry and texture-lint rules);
    // its size is pinned by regia_asset_pipeline's material_lint_coverage test.
    assert_eq!(
        read["work"]["rules_checked"],
        regia_asset_pipeline::source_lint::rules().len()
    );
    let preview = call(
        &mut world,
        "asset.content_lint_fix_preview",
        json!({"path": "assets/hero.gltf", "rule_id": "factor_missing"}),
    )
    .expect("preview");
    let href = preview["previews"][0]["fix_preview"].as_str().expect("href");
    assert!(href.starts_with("content-lint://fix-preview?"), "{href}");

    {
        let mut panels = world.resource_mut::<AssetPanels>();
        panels.problems.rows.push(ProblemRow {
            severity: Severity::Error,
            code: "dummy".to_string(),
            validator: "other".to_string(),
            asset: "assets/hero.gltf".to_string(),
            text: "not a source-lint row".to_string(),
            hint: None,
        });
    }
    let filtered = call(
        &mut world,
        "asset.content_lint_read",
        json!({"target": "mobile", "validator": "source_lint"}),
    )
    .expect("filtered read");
    let visible_codes: Vec<String> = {
        let panels = world.resource::<AssetPanels>();
        let visible = panels.problems.visible();
        assert!(visible.iter().all(|row| row.validator == "source_lint"));
        let mut codes: Vec<String> = visible.iter().map(|row| row.code.clone()).collect();
        codes.sort_unstable();
        codes
    };
    let mut rpc_ids: Vec<String> = filtered["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .map(|row| row["rule_id"].as_str().expect("rule").to_string())
        .collect();
    rpc_ids.sort_unstable();
    assert_eq!(rpc_ids, visible_codes);

    let project = content_lint_jobs::human_apply_thresholds(
        &mut world,
        "project",
        Some(GeometryOverlay {
            max_vertices: Some(64),
            ..GeometryOverlay::default()
        }),
        None,
        Some("assets/hero.gltf"),
    )
    .expect("project defaults");
    assert_eq!(project["target"], "default");
    assert_eq!(
        project["provenance"]["geometry"]["max_vertices"],
        "project_defaults"
    );
    assert_eq!(project["geometry"]["max_vertices"], 64);
    content_lint_jobs::dismiss_pending(&mut world);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn read_only_cannot_edit_thresholds_or_apply_fixes() {
    let dir = regia_test_utils::unique_temp_dir("content-lint-jobs-readonly");
    write_model(&dir, "assets/hero.gltf", BARE);
    let policy = b"(format_version: 1)\n";
    std::fs::create_dir_all(dir.join("config")).expect("config");
    regia_test_utils::write_atomic(&dir.join("config/source_lint.ron"), policy).expect("policy");
    let gltf_before = std::fs::read(dir.join("assets/hero.gltf")).expect("gltf");
    let policy_before = std::fs::read(dir.join("config/source_lint.ron")).expect("policy bytes");
    let mut world = open_world(&dir, true);
    for method in ["asset.content_lint_thresholds", "asset.content_lint_apply_fix"] {
        let err = call(
            &mut world,
            method,
            json!({"target": "mobile", "path": "assets/hero.gltf", "geometry": {"max_triangles": 1000}}),
        )
        .expect_err(method);
        assert_eq!(err.code, protocol::PERMISSION_DENIED);
        assert_eq!(err.data.expect("data")["code"], "F0012");
    }
    let human = content_lint_jobs::human_apply_thresholds(
        &mut world,
        "mobile",
        Some(GeometryOverlay {
            max_triangles: Some(1000),
            ..GeometryOverlay::default()
        }),
        None,
        Some("assets/hero.gltf"),
    )
    .expect_err("human denied");
    assert_eq!(human.code, protocol::PERMISSION_DENIED);
    assert_eq!(human.data.expect("data")["code"], "F0012");
    let human_fix =
        content_lint_jobs::human_apply_fix(&mut world, "assets/hero.gltf", None, None).expect_err("fix");
    assert_eq!(human_fix.code, protocol::PERMISSION_DENIED);
    assert_eq!(
        std::fs::read(dir.join("assets/hero.gltf")).expect("gltf"),
        gltf_before
    );
    assert_eq!(
        std::fs::read(dir.join("config/source_lint.ron")).expect("policy"),
        policy_before
    );

    let mut editing = open_world(&dir, false);
    let extension = editing.resource::<regia_agent::extension::AgentExtension>();
    for name in [
        "asset.content_lint_start",
        "asset.content_lint_cancel",
        "asset.content_lint_status",
        "asset.content_lint_read",
        "asset.content_lint_fix_preview",
    ] {
        let schema = extension.method_schema(name).expect(name);
        assert!(!schema.mutating, "{name}");
        assert_eq!(schema.permission, regia_agent::schema::Permission::Read);
    }
    for name in ["asset.content_lint_thresholds", "asset.content_lint_apply_fix"] {
        let schema = extension.method_schema(name).expect(name);
        assert!(schema.mutating, "{name}");
        assert_eq!(schema.permission, regia_agent::schema::Permission::Assets);
    }
    let _ = extension;
    let preview = call(
        &mut editing,
        "asset.content_lint_apply_fix",
        json!({"path": "assets/hero.gltf", "rule_id": "factor_missing", "location": "/materials/0"}),
    )
    .expect("preview only");
    assert_eq!(preview["ok"], false);
    assert_eq!(preview["code"], "content_lint_preview_only");
    assert_eq!(preview["reason"], "content_lint_preview_only");
    assert!(
        preview["fix_preview"]
            .as_str()
            .expect("href")
            .starts_with("content-lint://fix-preview?")
    );
    assert_eq!(
        std::fs::read(dir.join("assets/hero.gltf")).expect("gltf"),
        gltf_before
    );
    assert_eq!(
        std::fs::read(dir.join("config/source_lint.ron")).expect("policy"),
        policy_before
    );
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn synchronous_reads_name_the_job_above_the_ceiling() {
    let dir = regia_test_utils::unique_temp_dir("content-lint-jobs-sync");
    let mut big = vec![b' '; content_lint_jobs::SYNC_MAX_ENCODED_BYTES as usize + 1];
    big[..BARE.len()].copy_from_slice(BARE);
    write_model(&dir, "assets/big.gltf", &big);
    write_model(&dir, "assets/small.gltf", BARE);
    let mut world = open_world(&dir, false);
    for method in ["asset.gltf_lint", "asset.gltf_material_lint"] {
        let err = call(&mut world, method, json!({"path": "assets/big.gltf"})).expect_err(method);
        assert_eq!(err.data.expect("data")["reason"], "content_lint_limit");
        assert!(
            err.message.contains("asset.content_lint_start"),
            "{method}: {}",
            err.message
        );
    }
    call(
        &mut world,
        "asset.gltf_lint",
        json!({"path": "assets/small.gltf"}),
    )
    .expect("small geometry");
    call(
        &mut world,
        "asset.gltf_material_lint",
        json!({"path": "assets/small.gltf"}),
    )
    .expect("small material");
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[derive(Component, Reflect, Clone, Debug, Default)]
#[reflect(Component, Default)]
struct LintJobEvidence {
    findings: u64,
    sources_processed: u64,
    bytes_hashed: u64,
}

fn call_app(app: &mut App, method: &str, params: Value) -> Result<Value, RpcError> {
    match call_local(app.world_mut(), method, params, Some("content-lint-jobs")) {
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
fn headless_workflow_waits_for_scan_evidence() {
    for method in [
        "asset.content_lint_rules",
        "asset.content_lint_policy",
        "asset.content_lint_start",
        "asset.content_lint_cancel",
        "asset.content_lint_status",
        "asset.content_lint_read",
        "asset.content_lint_thresholds",
        "asset.content_lint_fix_preview",
        "asset.content_lint_apply_fix",
    ] {
        assert!(
            regia_agent::guide_mentions(method),
            "{method} missing from agent.guide"
        );
    }
    let dir = regia_test_utils::unique_temp_dir("content-lint-jobs-workflow");
    write_model(&dir, "assets/hero.gltf", EMPTY_MATERIAL);
    let file_len = std::fs::metadata(dir.join("assets/hero.gltf"))
        .expect("len")
        .len();
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(dir.clone()),
    });
    app.add_plugins(regia_agent::AgentPlugin);
    app.register_type::<LintJobEvidence>();
    regia_editor::agent_ext::install(app.world_mut());
    app.finish_plugins();
    app.update();
    let ping = content_lint_jobs::inject_done(app.world_mut());
    call_app(
        &mut app,
        "asset.content_lint_start",
        json!({"paths": ["assets/hero.gltf"], "target": "default"}),
    )
    .expect("start");
    done_ping(&ping);
    regia_editor::editor_jobs::tick(app.world_mut());
    let read = call_app(
        &mut app,
        "asset.content_lint_read",
        json!({"target": "default", "path": "assets/hero.gltf"}),
    )
    .expect("read");
    let findings = read["findings"].as_array().expect("findings").len() as u64;
    assert!(findings >= 1);
    let entity = app
        .world_mut()
        .spawn(LintJobEvidence {
            findings,
            sources_processed: read["work"]["sources_processed"].as_u64().expect("sources"),
            bytes_hashed: read["work"]["bytes_hashed"].as_u64().expect("bytes"),
        })
        .id();
    let selector = json!(entity.to_bits());
    let condition = json!({"all": [
        {"entity": selector, "component": "LintJobEvidence", "path": "findings", "op": "eq", "value": findings},
        {"entity": selector, "component": "LintJobEvidence", "path": "sources_processed", "op": "eq", "value": 1},
        {"entity": selector, "component": "LintJobEvidence", "path": "bytes_hashed", "op": "eq", "value": file_len}
    ]});
    let waited = call_app(&mut app, "play.wait_until", json!({"condition": condition})).expect("wait_until");
    assert_eq!(waited["satisfied"], true, "{waited}");
    app.world_mut().insert_resource(Time::default());
    let checked = call_app(
        &mut app,
        "play.expect",
        json!({"assertions": [
            {"name": "source-lint evidence", "condition": condition},
            {"name": "gameplay is not paused", "condition": {"resource": "Time", "path": "paused", "op": "eq", "value": false}}
        ]}),
    )
    .expect("expect");
    assert_eq!(checked["pass"], true, "{checked}");
    std::fs::remove_dir_all(dir).expect("cleanup");
}
