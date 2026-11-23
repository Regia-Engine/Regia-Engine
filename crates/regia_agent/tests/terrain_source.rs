// SPDX-License-Identifier: MPL-2.0
//! Count-source inspection and overview baking through the public agent dispatcher.

use regia_agent::protocol::RpcError;
use regia_agent::{LocalReply, call_local};
use regia_app::prelude::*;
use serde_json::{Value, json};

fn call_in_process(world: &mut World, method: &str, params: Value) -> Option<Result<Value, RpcError>> {
    match call_local(world, method, params, Some("terrain-source-test")) {
        LocalReply::Done(result) => Some(result),
        LocalReply::Pending(_) => None,
    }
}

fn inspect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    call_in_process(world, "terrain.inspect_source", params.clone()).expect("immediate")
}
fn bake(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    call_in_process(world, "terrain.bake_overview", params.clone()).expect("immediate")
}

fn fixture() -> Value {
    json!({"schema":2,"grid":{"cols":3,"rows":2,"cell_m":2.0},
        "units":{"kind":"counts","meters_per_count":0.25,"zero_counts":100},
        "counts":[96,100,104,108,112,116]})
}

fn app(root: &std::path::Path) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(regia_agent::AgentPermissions::default());
    app.insert_resource(
        regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.to_path_buf(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("server"),
    );
    app
}

#[test]
fn terrain_source_dispatch_inspects_and_bakes_north_up_png() {
    let root = regia_test_utils::unique_temp_dir("terrain_source_dispatch");
    std::fs::create_dir_all(&root).expect("root");
    regia_agent::bake_export::write_atomic(&root.join("source.json"), fixture().to_string().as_bytes())
        .expect("source");
    let mut app = app(&root);
    let result = call_in_process(
        app.world_mut(),
        "terrain.inspect_source",
        json!({"path":"source.json"}),
    )
    .expect("immediate")
    .expect("inspect");
    assert_eq!(result["corners_m"], json!([-1.0, 1.0, 2.0, 4.0]));
    assert_eq!(result["sample0"], json!([-3.0, -2.0]));
    let result = call_in_process(app.world_mut(), "terrain.bake_overview", json!({
        "path":"source.json", "output":"preview/map.png", "overview":{"size":[12,8],"blur_passes":0,"relief":0.0}
    })).expect("immediate").expect("bake");
    assert_eq!(result["north_up"], true);
    let bytes = std::fs::read(root.join("preview/map.png")).expect("PNG");
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .expect("header");
    assert_eq!((reader.info().width, reader.info().height), (12, 8));
    let mut pixels = vec![0; reader.output_buffer_size().expect("size")];
    reader.next_frame(&mut pixels).expect("pixels");
    assert!(pixels[0] > pixels[7 * 12 * 3], "north row is higher and brighter");
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn terrain_source_dispatch_read_only_refuses_bake_and_keeps_inspection() {
    let root = regia_test_utils::unique_temp_dir("terrain_source_permissions");
    std::fs::create_dir_all(&root).expect("root");
    regia_agent::bake_export::write_atomic(&root.join("source.json"), fixture().to_string().as_bytes())
        .expect("source");
    let mut app = app(&root);
    app.world_mut()
        .resource_mut::<regia_agent::AgentPermissions>()
        .config
        .read_only = true;
    assert!(
        call_in_process(
            app.world_mut(),
            "terrain.inspect_source",
            json!({"path":"source.json"})
        )
        .expect("immediate")
        .is_ok()
    );
    let error = call_in_process(
        app.world_mut(),
        "terrain.bake_overview",
        json!({"path":"source.json","output":"out.png"}),
    )
    .expect("immediate")
    .expect_err("read only");
    assert_eq!(error.data.expect("data")["code"], "F0012");
    assert!(!root.join("out.png").exists());
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn terrain_source_refusals_leave_no_output() {
    let root = regia_test_utils::unique_temp_dir("terrain_source_refusals");
    std::fs::create_dir_all(&root).expect("root");
    regia_agent::bake_export::write_atomic(&root.join("source.json"), fixture().to_string().as_bytes())
        .expect("source");
    let mut app = app(&root);
    for args in [
        json!({"path":"source.json","output":"../outside.png"}),
        json!({"path":"source.json","output":"out.png","overview":{"size":[u32::MAX,u32::MAX]}}),
        json!({"path":"source.json","output":"out.png","overview":{"relief":-1}}),
        json!({"path":"source.json","output":"out.png","png":{"cell_m":1}}),
    ] {
        assert!(bake(app.world_mut(), &args).is_err(), "{args}");
    }
    assert!(!root.join("out.png").exists());
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn terrain_source_bake_never_overwrites_its_input() {
    let root = regia_test_utils::unique_temp_dir("terrain_source_same_path");
    std::fs::create_dir_all(&root).expect("root");
    let bytes = b"source bytes preserved even if invalid";
    regia_agent::bake_export::write_atomic(&root.join("source.png"), bytes).expect("source");
    let mut app = app(&root);
    let result = bake(
        app.world_mut(),
        &json!({"path":"source.png","output":"source.png"}),
    );
    assert!(result.is_err());
    assert_eq!(std::fs::read(root.join("source.png")).expect("read"), bytes);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[cfg(unix)]
#[test]
fn terrain_source_symlinks_cannot_escape_the_project() {
    let root = regia_test_utils::unique_temp_dir("terrain_source_symlinks");
    let outside = regia_test_utils::unique_temp_dir("terrain_source_outside");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::create_dir_all(&outside).expect("outside");
    regia_agent::bake_export::write_atomic(&outside.join("source.json"), fixture().to_string().as_bytes())
        .expect("outside source");
    regia_agent::bake_export::write_atomic(&root.join("source.json"), fixture().to_string().as_bytes())
        .expect("inside source");
    std::os::unix::fs::symlink(&outside, root.join("away")).expect("symlink");
    let mut app = app(&root);
    assert!(inspect(app.world_mut(), &json!({"path":"away/source.json"})).is_err());
    assert!(
        bake(
            app.world_mut(),
            &json!({"path":"source.json","output":"away/new/out.png"})
        )
        .is_err()
    );
    assert!(!outside.join("new").exists());
    std::fs::remove_dir_all(root).expect("cleanup root");
    std::fs::remove_dir_all(outside).expect("cleanup outside");
}

#[test]
fn terrain_source_dispatch_png_uses_metadata_before_fallback() {
    let root = regia_test_utils::unique_temp_dir("terrain_source_png");
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 2, 2);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Sixteen);
        encoder
            .add_text_chunk("custom_datum".into(), "1000".into())
            .expect("datum");
        let mut writer = encoder.write_header().expect("header");
        let raw: Vec<_> = [992u16, 1000, 1008, 1024]
            .into_iter()
            .flat_map(u16::to_be_bytes)
            .collect();
        writer.write_image_data(&raw).expect("pixels");
        writer.finish().expect("finish");
    }
    regia_agent::bake_export::write_atomic(&root.join("source.png"), &bytes).expect("source");
    let mut app = app(&root);
    let result = call_in_process(
        app.world_mut(),
        "terrain.inspect_source",
        json!({
            "path":"source.png", "png":{"meters_per_count":0.125,"cell_m":3.0,
                "datum_keyword":"custom_datum","zero_counts":0}
        }),
    )
    .expect("immediate")
    .expect("source");
    assert_eq!(result["zero_counts"], 1000);
    assert_eq!(result["corners_m"], json!([-1.0, 0.0, 1.0, 3.0]));
    let error = call_in_process(
        app.world_mut(),
        "terrain.inspect_source",
        json!({
            "path":"source.png", "png":{"meters_per_count":0.125,"cell_m":3.0,
                "zero_counts":0,"center_zero":true}
        }),
    )
    .expect("immediate")
    .expect_err("conflicting fallback");
    assert_eq!(error.data.expect("data")["code"], "F0002");
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn terrain_source_methods_have_schema_contract_and_discovery_coverage() {
    use regia_agent::schema::Permission;
    for (name, permission, mutating) in [
        ("terrain.inspect_source", Permission::Read, false),
        ("terrain.bake_overview", Permission::Assets, true),
    ] {
        let schema = regia_agent::schema::method(name).expect("schema");
        assert_eq!(schema.permission, permission);
        assert_eq!(schema.mutating, mutating);
        assert_eq!(
            regia_agent::contract::CONTRACTS
                .iter()
                .filter(|c| c.method == name)
                .count(),
            1
        );
        let contract = regia_agent::contract::contract(name).expect("contract");
        assert_eq!(contract.cost, regia_agent::contract::Cost::Heavy);
        for code in [
            "height_source_limit_bytes",
            "height_source_limit_samples",
            "height_source_limit_depth",
            "height_source_limit_metadata",
            "height_source_allocation",
        ] {
            assert!(contract.errors.contains(&code), "{name} missing {code}");
        }
        assert!(schema.params.iter().any(|p| p.name == "limits"));
        assert!(
            regia_agent::GUIDE_TOPICS
                .iter()
                .any(|topic| topic.text.contains(name))
        );
    }
}

#[test]
fn request_limits_and_project_policy_report_effective_ceilings() {
    use regia_agent::TerrainSourcePolicy;
    use regia_terrain::source::ImportLimits;
    let root = regia_test_utils::unique_temp_dir("terrain_source_policy");
    regia_agent::bake_export::write_atomic(&root.join("source.json"), fixture().to_string().as_bytes())
        .expect("source");
    let mut app = app(&root);
    app.insert_resource(TerrainSourcePolicy {
        limits: ImportLimits {
            max_samples: 6,
            max_unknown_values: 12,
            ..Default::default()
        },
    });
    let report = inspect(
        app.world_mut(),
        &json!({"path":"source.json", "limits":{"max_samples":100,"max_unknown_values":100}}),
    )
    .expect("clamped");
    assert_eq!(report["effective_limits"]["max_samples"], 6);
    assert_eq!(report["effective_limits"]["max_unknown_values"], 12);
    let baked = bake(app.world_mut(), &json!({"path":"source.json","output":"preview.png","overview":{"size":[2,2]},"limits":{"max_samples":6}})).expect("bake");
    assert_eq!(baked["source"]["effective_limits"], report["effective_limits"]);
    for (params, code) in [
        (
            json!({"path":"source.json","limits":{"max_samples":4}}),
            "height_source_limit_samples",
        ),
        (
            json!({"path":"source.json","limits":{"max_bytes":4}}),
            "height_source_limit_bytes",
        ),
        (
            json!({"path":"source.json","limits":{"max_json_depth":2}}),
            "height_source_limit_depth",
        ),
    ] {
        let error = inspect(app.world_mut(), &params).expect_err("budget");
        let data = error.data.expect("data");
        assert_eq!(data["code"], code);
        assert_eq!(data["effective_limits"]["max_unknown_values"], 12);
    }
    for bad in [
        json!({"max_samples":-1}),
        json!({"max_samples":1.5}),
        json!({"unknown":4}),
        Value::Null,
    ] {
        assert!(inspect(app.world_mut(), &json!({"path":"source.json","limits":bad})).is_err());
    }
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn budget_refusal_preserves_output_and_scene_through_wait_until_and_expect() {
    use regia_scene::Transform;
    let root = regia_test_utils::unique_temp_dir("terrain_source_budget_workflow");
    regia_agent::bake_export::write_atomic(&root.join("source.json"), fixture().to_string().as_bytes())
        .expect("source");
    regia_agent::bake_export::write_atomic(&root.join("preview.png"), b"previous preview").expect("preview");
    let mut app = app(&root);
    // Install the real agent schedules. No finish_plugins/server/socket is needed
    // for call_local; this workflow advances one deterministic in-process frame.
    app.add_plugins(regia_agent::AgentPlugin)
        .register_type::<Transform>();
    let entity = app.world_mut().spawn(Transform::default()).id().to_bits();
    let condition =
        json!({"entity":entity,"component":"Transform","path":"translation.y","op":"eq","value":0.0});
    let LocalReply::Pending(receiver) = call_local(
        app.world_mut(),
        "play.wait_until",
        json!({"condition":condition,"timeout_ms":5000}),
        Some("terrain-budget-workflow"),
    ) else {
        panic!("wait must queue");
    };
    let error = bake(
        app.world_mut(),
        &json!({"path":"source.json","output":"preview.png","limits":{"max_samples":4}}),
    )
    .expect_err("budget");
    assert_eq!(error.data.expect("data")["code"], "height_source_limit_samples");
    app.update();
    let response = receiver.try_recv().expect("fixed-step wait answered");
    assert_eq!(response.result.expect("wait")["satisfied"], true);
    let result = call_in_process(
        app.world_mut(),
        "play.expect",
        json!({"assertions":[{"name":"refused import leaves scene unchanged","condition":condition}]}),
    )
    .expect("immediate")
    .expect("expect report");
    assert_eq!(result["pass"], true);
    assert_eq!(
        std::fs::read(root.join("preview.png")).expect("preview"),
        b"previous preview"
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn terrain_source_fuzz_registry_has_bounded_valid_seeds_and_mutations() {
    use regia_agent::fuzz_targets::{self, FuzzInput};
    for name in ["terrain.source_json", "terrain.source_png"] {
        let target = fuzz_targets::target(name).expect("registered");
        match target.input {
            FuzzInput::Text { fixture, load } => {
                load(&fixture()).expect("JSON seed");
            }
            FuzzInput::Bytes { fixture, load } => {
                load(&fixture()).expect("PNG seed");
            }
        }
        let report = fuzz_targets::run(target, None, 8237, 256).expect("mutations");
        assert!(report.is_clean(), "{name}: {report:?}");
    }
}

#[test]
fn terrain_source_budget_codes_have_agent_explanations() {
    let mut world = World::new();
    for code in [
        "height_source_allocation",
        "height_source_cancelled",
        "height_source_limit_bytes",
        "height_source_limit_depth",
        "height_source_limit_metadata",
        "height_source_limit_samples",
    ] {
        let explanation = call_in_process(&mut world, "errors.explain", json!({"code":code}))
            .expect("immediate")
            .expect("documented refusal");
        assert_eq!(explanation["code"], code);
        assert_eq!(explanation["jsonrpc"], regia_agent::protocol::INVALID_PARAMS);
        assert!(!explanation["meaning"].as_str().expect("meaning").is_empty());
        assert!(!explanation["fix"].as_str().expect("fix").is_empty());
    }
}

/// ADR-8471: `terrain.inspect_source` with a cook `mapping` (a schema-2 `heightfield.json`)
/// previews exactly what `regia cook` persists: the mapping, the world bounds and whether a level
/// will load it, before anything is applied. The scene is checked unchanged through
/// `play.wait_until` and `play.expect`.
#[test]
fn inspect_with_a_cook_mapping_previews_world_bounds_before_apply() {
    use regia_scene::Transform;
    let root = regia_test_utils::unique_temp_dir("terrain_source_cook_mapping");
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 3, 3);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Sixteen);
        let mut writer = encoder.write_header().expect("header");
        let raw: Vec<_> = [992u16, 1000, 1008, 1016, 1024, 1032, 1040, 1048, 1056]
            .into_iter()
            .flat_map(u16::to_be_bytes)
            .collect();
        writer.write_image_data(&raw).expect("pixels");
        writer.finish().expect("finish");
    }
    regia_agent::bake_export::write_atomic(&root.join("heightfield.png"), &bytes).expect("png");
    let mapping = json!({"schema":2,
        "grid":{"cols":3,"rows":3,"cell_m":4.0,"sample0_x_m":10.0,"sample0_z_m":10.0},
        "units":{"kind":"counts","meters_per_count":0.125,"zero_counts":1000}});
    regia_agent::bake_export::write_atomic(&root.join("heightfield.json"), mapping.to_string().as_bytes())
        .expect("mapping");
    let rect = json!({"schema":2,
        "grid":{"cols":3,"rows":3,"cell_m":4.0,"sample0_x_m":10.0,"sample0_z_m":11.0},
        "units":{"kind":"counts","meters_per_count":0.125,"zero_counts":1000}});
    regia_agent::bake_export::write_atomic(&root.join("skew.json"), rect.to_string().as_bytes())
        .expect("skew");

    let mut app = app(&root);
    app.add_plugins(regia_agent::AgentPlugin)
        .register_type::<Transform>();
    let entity = app.world_mut().spawn(Transform::default()).id().to_bits();
    let condition =
        json!({"entity":entity,"component":"Transform","path":"translation.y","op":"eq","value":0.0});
    let LocalReply::Pending(receiver) = call_local(
        app.world_mut(),
        "play.wait_until",
        json!({"condition":condition,"timeout_ms":5000}),
        Some("terrain-mapping-workflow"),
    ) else {
        panic!("wait must queue");
    };
    // The PNG's own settings are placeholders: the mapping file replaces them, as the cook does.
    let png = json!({"meters_per_count":1.0,"cell_m":1.0,"zero_counts":0});
    let report = inspect(
        app.world_mut(),
        &json!({"path":"heightfield.png","png":png,"mapping":"heightfield.json"}),
    )
    .expect("preview");
    assert_eq!(report["zero_counts"], 1000);
    assert_eq!(report["corners_m"], json!([-1.0, 1.0, 5.0, 7.0]));
    assert_eq!(report["world_min"], json!([10.0, 10.0]));
    assert_eq!(report["world_max"], json!([18.0, 18.0]));
    assert_eq!(report["mapping"]["sample0_m"], json!([10.0, 10.0]));
    assert_eq!(report["mapping"]["cell_m"], 4.0);
    assert_eq!(report["runtime_map"]["loadable"], true);
    assert_eq!(report["runtime_map"]["grid_origin"], json!([14.0, 14.0]));
    let skew = inspect(
        app.world_mut(),
        &json!({"path":"heightfield.png","png":png,"mapping":"skew.json"}),
    )
    .expect("preview of an unloadable mapping");
    assert_eq!(skew["runtime_map"]["loadable"], false);
    assert!(
        skew["runtime_map"]["reason"]
            .as_str()
            .expect("reason")
            .contains("differs on X and Z")
    );
    let legacy_root = root.join("legacy.json");
    regia_agent::bake_export::write_atomic(&legacy_root, br#"{"size_m": 8}"#).expect("legacy");
    let error = inspect(
        app.world_mut(),
        &json!({"path":"heightfield.png","png":png,"mapping":"legacy.json"}),
    )
    .expect_err("no mapping in a legacy file");
    assert_eq!(error.data.expect("data")["code"], "height_io_decode");

    app.update();
    let response = receiver.try_recv().expect("fixed-step wait answered");
    assert_eq!(response.result.expect("wait")["satisfied"], true);
    let result = call_in_process(
        app.world_mut(),
        "play.expect",
        json!({"assertions":[{"name":"a preview leaves the scene unchanged","condition":condition}]}),
    )
    .expect("immediate")
    .expect("expect report");
    assert_eq!(result["pass"], true);
    std::fs::remove_dir_all(root).expect("cleanup");
}

/// ADR-8542: `terrain.inspect_source` / `terrain.bake_overview` with a `raster` descriptor show
/// the mapping axes, the registration, unequal X/Z spacing and the missing cells before anything
/// is applied; a bad mask is refused with its code. The scene is checked unchanged through
/// `play.wait_until` and `play.expect`.
#[test]
fn raster_descriptor_previews_axes_registration_and_missing_cells() {
    use regia_scene::Transform;
    let root = regia_test_utils::unique_temp_dir("terrain_source_raster_metadata");
    std::fs::create_dir_all(&root).expect("root");
    // Stored north-first: top row is maximum Z. 65535 marks no data.
    let source = json!({"schema":2,"grid":{"cols":3,"rows":2,"cell_m":1.0},
        "units":{"kind":"counts","meters_per_count":0.25,"zero_counts":100},
        "counts":[108,65535,116,96,100,104]});
    regia_agent::bake_export::write_atomic(&root.join("north.json"), source.to_string().as_bytes())
        .expect("source");
    let descriptor = json!({"raster_descriptor":1,"spacing_m":[2.0,0.5],"row0":"north",
        "registration":"edge","origin_m":[10.0,-4.0],"nodata_counts":65535});
    regia_agent::bake_export::write_atomic(
        &root.join("north.raster.json"),
        descriptor.to_string().as_bytes(),
    )
    .expect("descriptor");

    let mut app = app(&root);
    app.add_plugins(regia_agent::AgentPlugin)
        .register_type::<Transform>();
    let entity = app.world_mut().spawn(Transform::default()).id().to_bits();
    let condition =
        json!({"entity":entity,"component":"Transform","path":"translation.y","op":"eq","value":0.0});
    let LocalReply::Pending(receiver) = call_local(
        app.world_mut(),
        "play.wait_until",
        json!({"condition":condition,"timeout_ms":5000}),
        Some("terrain-raster-workflow"),
    ) else {
        panic!("wait must queue");
    };
    let report = inspect(
        app.world_mut(),
        &json!({"path":"north.json","raster":"north.raster.json"}),
    )
    .expect("preview");
    // Canonical south-west first: the stored bottom row (96 100 104) is row 0.
    assert_eq!(report["corners_m"], json!([-1.0, 1.0, 2.0, 4.0]));
    assert_eq!(report["spacing_m"], json!([2.0, 0.5]));
    assert_eq!(report["sample0"], json!([11.0, -3.75]));
    assert_eq!(report["world_max"], json!([15.0, -3.25]));
    assert_eq!(report["min_m"], -1.0);
    assert_eq!(report["max_m"], 4.0, "the sentinel is not a height");
    assert_eq!(report["missing_samples"], 1);
    assert_eq!(report["raster"]["declared"]["row0"], "north");
    assert_eq!(report["raster"]["registration"], "edge");
    assert_eq!(report["raster"]["missing_cells"], json!([[1, 1]]));
    assert_eq!(
        report["mapping"],
        Value::Null,
        "unequal spacing has no count mapping"
    );
    assert_eq!(report["runtime_map"]["loadable"], false);
    assert!(
        report["runtime_map"]["reason"]
            .as_str()
            .expect("reason")
            .starts_with("terrain_raster_missing_data")
    );
    let baked = bake(
        app.world_mut(),
        &json!({"path":"north.json","raster":descriptor,"output":"preview/north.png",
            "overview":{"size":[3,2],"blur_passes":0}}),
    )
    .expect("bake");
    assert_eq!(baked["missing_pixels"], 1);
    assert_eq!(baked["missing_rgb"], json!([255, 0, 255]));
    let bad = json!({"raster_descriptor":1,"spacing_m":[1.0,1.0],"registration":"center",
        "valid_mask":{"cols":2,"rows":3,"valid":[1,1,1,1,1,1]}});
    let error = inspect(app.world_mut(), &json!({"path":"north.json","raster":bad})).expect_err("bad mask");
    assert_eq!(error.data.expect("data")["code"], "terrain_raster_mask");
    let sheared =
        json!({"raster_descriptor":1,"spacing_m":[1.0,1.0],"registration":"center","skew_m":[0.5,0.0]});
    let error =
        inspect(app.world_mut(), &json!({"path":"north.json","raster":sheared})).expect_err("sheared");
    assert_eq!(error.data.expect("data")["code"], "terrain_raster_transform");

    app.update();
    let response = receiver.try_recv().expect("fixed-step wait answered");
    assert_eq!(response.result.expect("wait")["satisfied"], true);
    let result = call_in_process(
        app.world_mut(),
        "play.expect",
        json!({"assertions":[{"name":"a raster preview leaves the scene unchanged","condition":condition}]}),
    )
    .expect("immediate")
    .expect("expect report");
    assert_eq!(result["pass"], true);
    std::fs::remove_dir_all(root).expect("cleanup");
}

/// ADR-8595: a `*.tiles.json` source tile set previews like the equivalent monolithic source and
/// reports its coverage and counters; a disagreeing shared border answers `terrain_tiles_seam`
/// with the sample and both tiles, a gap `terrain_tiles_gap`; an oversized monolithic source
/// carries a hint to split it. Ordered by `play.wait_until`, checked by `play.expect`.
#[test]
fn source_tile_set_previews_coverage_and_refuses_seams_and_gaps() {
    use regia_scene::Transform;
    let root = regia_test_utils::unique_temp_dir("terrain_source_tile_set");
    std::fs::create_dir_all(root.join("tiles")).expect("root");
    let count = |c: u32, r: u32| 1000 + c * 7 + r * 31;
    // A 5x3 grid in two tiles sharing column 2; the monolithic equivalent beside it.
    let tile = |x0: u32, w: u32, seam_bump: u32| {
        let counts: Vec<u32> = (0..3)
            .flat_map(|r| (x0..x0 + w).map(move |c| (c, r)))
            .map(|(c, r)| count(c, r) + if c == 2 && r == 1 { seam_bump } else { 0 })
            .collect();
        json!({"schema":2,"grid":{"cols":w,"rows":3,"cell_m":2.0,
            "sample0_x_m": -4.0 + 2.0 * x0 as f32, "sample0_z_m": -2.0},
            "units":{"kind":"counts","meters_per_count":0.25,"zero_counts":1000},"counts":counts})
    };
    let write = |path: &str, value: &Value| {
        regia_agent::bake_export::write_atomic(&root.join(path), value.to_string().as_bytes()).expect("write")
    };
    write("tiles/west.json", &tile(0, 3, 0));
    write("tiles/east.json", &tile(2, 3, 0));
    let mono: Vec<u32> = (0..3).flat_map(|r| (0..5).map(move |c| count(c, r))).collect();
    write(
        "mono.json",
        &json!({"schema":2,"grid":{"cols":5,"rows":3,"cell_m":2.0,"sample0_x_m":-4.0,"sample0_z_m":-2.0},
            "units":{"kind":"counts","meters_per_count":0.25,"zero_counts":1000},"counts":mono}),
    );
    let descriptor = |east_origin: u32| {
        json!({"terrain_source_tiles":1,
            "grid":{"cols":5,"rows":3,"cell_m":2.0,"sample0_m":[-4.0,-2.0]},
            "units":{"meters_per_count":0.25,"zero_counts":1000},
            "tiles":[{"path":"tiles/east.json","origin":[east_origin,0],"size":[3,3]},
                     {"path":"tiles/west.json","origin":[0,0],"size":[3,3]}]})
    };
    write("height.tiles.json", &descriptor(2));

    let mut app = app(&root);
    app.add_plugins(regia_agent::AgentPlugin)
        .register_type::<Transform>();
    let entity = app.world_mut().spawn(Transform::default()).id().to_bits();
    let condition =
        json!({"entity":entity,"component":"Transform","path":"translation.y","op":"eq","value":0.0});
    let LocalReply::Pending(receiver) = call_local(
        app.world_mut(),
        "play.wait_until",
        json!({"condition":condition,"timeout_ms":5000}),
        Some("terrain-tile-set-workflow"),
    ) else {
        panic!("wait must queue");
    };

    let tiled = inspect(app.world_mut(), &json!({"path":"height.tiles.json"})).expect("tile-set preview");
    let monolithic = inspect(app.world_mut(), &json!({"path":"mono.json"})).expect("mono preview");
    for key in [
        "width",
        "height",
        "corners_m",
        "min_m",
        "max_m",
        "world_min",
        "world_max",
        "mapping",
    ] {
        assert_eq!(tiled[key], monolithic[key], "{key}");
    }
    let set = &tiled["tile_set"];
    assert_eq!(set["tiles"], 2);
    assert_eq!(set["covered"], true);
    assert_eq!(set["assembled"], true);
    assert_eq!(set["overlapping_pairs"], 1);
    assert_eq!(
        set["seam_samples_checked"], 3,
        "column 2 is shared on all three rows"
    );
    assert_eq!(
        set["tiles_listed"][0]["id"], "r0_c0",
        "sorted by origin, not file order"
    );

    write("tiles/east.json", &tile(2, 3, 5));
    let error = inspect(app.world_mut(), &json!({"path":"height.tiles.json"})).expect_err("seam");
    assert!(error.message.contains("(col 2, row 1)"), "{}", error.message);
    assert_eq!(error.data.expect("data")["code"], "terrain_tiles_seam");
    write("tiles/east.json", &tile(2, 3, 0));
    write("gap.tiles.json", &descriptor(3));
    let error = inspect(app.world_mut(), &json!({"path":"gap.tiles.json"})).expect_err("gap");
    assert_eq!(error.data.expect("data")["code"], "terrain_tiles_bounds");
    let mut gap = descriptor(2);
    gap["tiles"][0]["origin"] = json!([3, 0]);
    gap["tiles"][0]["size"] = json!([2, 3]);
    gap["tiles"][1]["size"] = json!([2, 3]); // west now ends at column 2: column 2 is in no tile
    write("gap.tiles.json", &gap);
    let error = inspect(app.world_mut(), &json!({"path":"gap.tiles.json"})).expect_err("gap");
    assert_eq!(error.data.expect("data")["code"], "terrain_tiles_gap");

    // A monolithic source over the request's sample budget: refused with the split hint.
    let error = inspect(
        app.world_mut(),
        &json!({"path":"mono.json","limits":{"max_samples":8}}),
    )
    .expect_err("budget");
    let data = error.data.expect("data");
    assert_eq!(data["code"], "height_source_limit_samples");
    assert!(
        data["hint"]
            .as_str()
            .expect("hint")
            .contains("heightfield.tiles.json")
    );
    // The same budget still reads the tile set's tiles (each 9 samples > 8 is refused per tile,
    // so give 9): the whole 15-sample grid is streamed, not assembled.
    let streamed = inspect(
        app.world_mut(),
        &json!({"path":"height.tiles.json","limits":{"max_samples":9}}),
    )
    .expect("streamed preview");
    assert_eq!(streamed["tile_set"]["assembled"], false);
    assert_eq!(streamed["max_m"], monolithic["max_m"]);

    app.update();
    let response = receiver.try_recv().expect("fixed-step wait answered");
    assert_eq!(response.result.expect("wait")["satisfied"], true);
    let result = call_in_process(
        app.world_mut(),
        "play.expect",
        json!({"assertions":[{"name":"a tile-set preview leaves the scene unchanged","condition":condition}]}),
    )
    .expect("immediate")
    .expect("expect report");
    assert_eq!(result["pass"], true);
    std::fs::remove_dir_all(root).expect("cleanup");
}
