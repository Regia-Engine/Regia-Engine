// SPDX-License-Identifier: MPL-2.0
//! MaterialX import (ADR-3341): the hand-written fixtures in `tests/fixtures/materialx/` import
//! to material graphs that compile through `regia_matgraph` to WGSL that validates with naga
//! in every host shader; parameter defaults match the documents; the unsupported-node document
//! lists its nodes; a malformed document returns its error code; and a project with a `.mtlx`
//! and its images cooks into a cooked material graph with the images as RGBA8 sub-assets.

use std::path::{Path, PathBuf};

use regia_asset::{AssetManifest, cooked_cache_dir, cooked_file_name, decode_cooked_file};
use regia_asset_pipeline::materialx_import::{
    MaterialXImport, MaterialXSettings, WARN_LOSSY_INPUT, WARN_UNSUPPORTED_NODE, agent_report,
    import_materialx_str,
};
use regia_asset_pipeline::{CookOptions, cook_project};
use regia_matgraph::{
    ColorSpace, CookedMaterialGraph, GraphAlphaMode, Node, NormalSpace, ParamValue, compile,
};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/materialx")
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(fixture_dir().join(name)).expect("fixture")
}

fn import(name: &str) -> MaterialXImport {
    import_materialx_str(&fixture(name), "assets/materials", &MaterialXSettings::default())
        .unwrap_or_else(|e| panic!("{name}: {} {e}", e.code()))
}

fn param(i: &MaterialXImport, name: &str) -> Option<ParamValue> {
    i.graph.params.iter().find(|p| p.name == name).map(|p| p.value)
}

/// Every fixture's graph compiles with validation (naga, every host module).
fn compiles(i: &MaterialXImport) {
    let compiled = compile(&i.graph).unwrap_or_else(|e| panic!("{}: {e}", i.report.material));
    assert!(compiled.wgsl.contains("fn material_surface"));
}

#[test]
fn a_textured_standard_surface_imports_its_images_normal_map_and_coat() {
    let i = import("textured_standard_surface.mtlx");
    compiles(&i);
    assert_eq!(i.report.material, "M_wood");
    assert_eq!(i.report.shader, "standard_surface");
    let textures: Vec<(&str, ColorSpace)> = i
        .graph
        .textures
        .iter()
        .map(|t| (t.path.as_str(), t.color_space))
        .collect();
    assert_eq!(
        textures,
        vec![
            ("assets/materials/textures/wood_albedo.png", ColorSpace::Srgb),
            ("assets/materials/textures/wood_roughness.png", ColorSpace::Linear),
            ("assets/materials/textures/wood_normal.png", ColorSpace::Linear),
        ]
    );
    // The normal map drives the tangent-space normal output.
    assert_eq!(i.graph.output.normal_space, NormalSpace::Tangent);
    let normal = &i.graph.nodes[i.graph.output.normal.as_ref().expect("normal")];
    assert!(matches!(normal, Node::Normalize(_)), "{normal:?}");
    assert!(
        i.graph
            .nodes
            .values()
            .any(|n| matches!(n, Node::NormalStrength { .. }))
    );
    assert!(
        i.graph
            .nodes
            .values()
            .any(|n| matches!(n, Node::UvTransform { scale, .. } if *scale == [2.0, 2.0]))
    );
    // The constant coat lands in the shading block; metalness is an exposed parameter.
    assert!((i.graph.shading.clearcoat - 0.3).abs() < 1e-6);
    assert!((i.graph.shading.clearcoat_roughness - 0.05).abs() < 1e-6);
    assert_eq!(param(&i, "metalness"), Some(ParamValue::Float(0.0)));
    assert_eq!(
        param(&i, "base_color"),
        None,
        "a connected input is not a parameter"
    );
    assert!(i.report.unsupported().next().is_none(), "{:?}", i.report.warnings);
}

#[test]
fn a_nodegraph_with_mix_and_multiply_exposes_its_interface_with_the_documents_defaults() {
    let i = import("nodegraph_mix_multiply.mtlx");
    compiles(&i);
    assert_eq!(param(&i, "tint"), Some(ParamValue::Color([0.9, 0.2, 0.1])));
    assert_eq!(param(&i, "dirt_amount"), Some(ParamValue::Float(0.35)));
    // Interface inputs take slots before the shader's own inputs.
    assert_eq!(i.graph.params[0].name, "tint");
    assert_eq!(i.graph.params[1].name, "dirt_amount");
    assert!(
        i.graph.nodes.values().any(|n| matches!(n, Node::Lerp { .. })),
        "mix → Lerp"
    );
    assert!(
        i.graph.nodes.values().any(|n| matches!(n, Node::Mul(..))),
        "multiply → Mul"
    );
    assert!(
        i.graph.nodes.values().any(|n| matches!(n, Node::Noise { .. })),
        "noise2d → Noise"
    );
    // The ifgreater has constant operands: a static choice of in1 (0.45).
    let rough = &i.graph.nodes[i.graph.output.roughness.as_ref().expect("roughness")];
    assert_eq!(rough, &Node::Float(0.45));
    assert!(
        i.report
            .mapped
            .iter()
            .any(|m| m.node == "NG_paint/rough_pick" && m.mapped_to.starts_with("static"))
    );
    let mapped: Vec<&str> = i.report.mapped.iter().map(|m| m.category.as_str()).collect();
    for category in [
        "mix",
        "multiply",
        "noise2d",
        "constant",
        "texcoord",
        "ifgreater",
        "standard_surface",
    ] {
        assert!(mapped.contains(&category), "{category} not in {mapped:?}");
    }
    // The instance block the compiler builds holds the defaults.
    let compiled = compile(&i.graph).expect("compiles");
    let slot = compiled.param("dirt_amount").expect("slot").slot;
    let block = regia_matgraph::default_params(&i.graph, 0.0);
    assert!((block[slot][0] - 0.35).abs() < 1e-6);
}

#[test]
fn an_open_pbr_surface_maps_its_weights_emission_in_nits_and_opacity() {
    let i = import("open_pbr_surface.mtlx");
    compiles(&i);
    assert_eq!(i.report.shader, "open_pbr_surface");
    assert_eq!(
        param(&i, "base_color"),
        Some(ParamValue::Color([1.0, 0.78, 0.34]))
    );
    assert_eq!(param(&i, "base_metalness"), Some(ParamValue::Float(1.0)));
    assert_eq!(param(&i, "specular_roughness"), Some(ParamValue::Float(0.25)));
    assert_eq!(param(&i, "emission_luminance"), Some(ParamValue::Float(12.0)));
    assert_eq!(
        param(&i, "emission_color"),
        Some(ParamValue::Color([1.0, 0.5, 0.2]))
    );
    assert_eq!(param(&i, "geometry_opacity"), Some(ParamValue::Float(0.8)));
    assert!(i.graph.output.emissive.is_some());
    assert!(i.graph.output.alpha.is_some());
    assert_eq!(i.graph.shading.alpha_mode, GraphAlphaMode::Blend);
    assert!((i.graph.shading.clearcoat - 0.5).abs() < 1e-6);
    assert!((i.graph.shading.clearcoat_roughness - 0.1).abs() < 1e-6);
    // The fuzz lobe has no counterpart: one lossy warning naming the input.
    assert!(
        i.report
            .warnings
            .iter()
            .any(|w| w.code == WARN_LOSSY_INPUT && w.node == "SR_gold.fuzz_weight")
    );
    // Fuzz colour and thin film are dropped too, each with its own warning (spec v1.39 inputs).
    for input in ["fuzz_color", "thin_film_weight"] {
        assert!(
            i.report
                .warnings
                .iter()
                .any(|w| w.code == WARN_LOSSY_INPUT && w.node == format!("SR_gold.{input}")),
            "{input} not warned: {:?}",
            i.report.warnings
        );
    }
}

#[test]
fn unsupported_nodes_warn_once_each_and_the_material_still_compiles() {
    let i = import("unsupported_node.mtlx");
    compiles(&i);
    let unsupported: Vec<(&str, &str)> = i
        .report
        .unsupported()
        .map(|w| (w.node.as_str(), w.category.as_str()))
        .collect();
    assert_eq!(unsupported.len(), 2, "{unsupported:?}");
    assert!(unsupported.contains(&("NG_cells/cells", "cellnoise2d")));
    assert!(unsupported.contains(&("NG_cells/hue", "hsvtocolor")));
    assert!(
        i.report
            .warnings
            .iter()
            .filter(|w| w.code == WARN_UNSUPPORTED_NODE)
            .all(|w| w.message.contains("stands in"))
    );
    assert!(i.report.mapped.iter().any(|m| m.category == "multiply"));
}

#[test]
fn a_custom_nodedef_is_flattened_with_its_inputs_as_parameters() {
    let i = import("custom_nodedef.mtlx");
    compiles(&i);
    assert_eq!(
        param(&i, "rusty_base"),
        Some(ParamValue::Color([0.7, 0.7, 0.75])),
        "instance value"
    );
    assert_eq!(
        param(&i, "rusty_amount"),
        Some(ParamValue::Float(0.5)),
        "nodedef default"
    );
    assert_eq!(param(&i, "metalness"), Some(ParamValue::Float(0.6)));
    assert!(
        i.report
            .mapped
            .iter()
            .any(|m| m.node == "rusty" && m.mapped_to.contains("ND_rust_color3"))
    );
}

#[test]
fn a_malformed_document_returns_an_error_code() {
    let e = import_materialx_str(
        &fixture("malformed.mtlx"),
        "assets",
        &MaterialXSettings::default(),
    )
    .expect_err("malformed");
    assert_eq!(e.code(), "mtlx_parse");
}

#[test]
fn every_fixture_imports_identically_twice() {
    for name in [
        "textured_standard_surface.mtlx",
        "nodegraph_mix_multiply.mtlx",
        "open_pbr_surface.mtlx",
        "unsupported_node.mtlx",
        "custom_nodedef.mtlx",
    ] {
        let a = import(name);
        let b = import(name);
        assert_eq!(a.graph, b.graph, "{name}: the import is deterministic");
        assert_eq!(a.report, b.report);
        // The graph survives the .matgraph.ron round trip (an editor can save it).
        let text = a.graph.to_ron().expect("ron");
        assert_eq!(
            regia_matgraph::MaterialGraph::from_ron(&text).expect("reparse"),
            a.graph,
            "{name}"
        );
    }
}

fn write_png(path: &Path, rgba: [u8; 4]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    let pixels: Vec<u8> = rgba.iter().copied().cycle().take(4 * 4 * 4).collect();
    image::RgbaImage::from_raw(4, 4, pixels)
        .expect("image")
        .save(path)
        .expect("png");
}

fn project() -> PathBuf {
    let root = regia_test_utils::unique_temp_dir("materialx_cook");
    let materials = root.join("assets/materials");
    std::fs::create_dir_all(&materials).expect("materials");
    for name in ["textured_standard_surface.mtlx", "unsupported_node.mtlx"] {
        std::fs::write(materials.join(name), fixture(name)).expect("fixture copy");
    }
    write_png(&materials.join("textures/wood_albedo.png"), [180, 120, 70, 255]);
    write_png(
        &materials.join("textures/wood_roughness.png"),
        [150, 150, 150, 255],
    );
    write_png(&materials.join("textures/wood_normal.png"), [128, 128, 255, 255]);
    root
}

#[test]
fn a_mtlx_source_cooks_to_a_material_graph_with_its_images() {
    let root = project();
    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert!(report.errors.is_empty(), "cook errors: {:?}", report.errors);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("mtlx_unsupported_node") && w.contains("NG_cells/cells")),
        "{:?}",
        report.warnings
    );
    let cache = cooked_cache_dir(&root, regia_asset_pipeline::host_platform());
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("present");
    let id = manifest
        .id_for_path("assets/materials/textured_standard_surface.mtlx")
        .expect("the .mtlx is in the manifest");
    let bytes = std::fs::read(cache.join(cooked_file_name(id))).expect("cooked file");
    let (_, payload) = decode_cooked_file(&bytes).expect("decodes");
    let graph = CookedMaterialGraph::from_postcard(payload).expect("a cooked material graph");
    assert_eq!(graph.textures.len(), 3);
    for image in ["wood_albedo.png", "wood_roughness.png", "wood_normal.png"] {
        let path = format!("assets/materials/textures/{image}#rgba8");
        assert!(
            manifest.id_for_path(&path).is_some(),
            "{path} cooked as a sub-asset"
        );
    }
    // The cook wrote a .meta with the importer id; a second cook with nothing changed skips.
    let meta = std::fs::read_to_string(root.join("assets/materials/textured_standard_surface.mtlx.meta"))
        .expect("meta");
    assert!(meta.contains("materialx"), "{meta}");
    let again = cook_project(CookOptions::for_host(root.clone())).expect("recook");
    assert!(again.errors.is_empty(), "{:?}", again.errors);
    assert_eq!(again.cooked, 0, "nothing changed: {again:?}");

    // The agent report reads the same file.
    let body = agent_report(&root, "assets/materials/unsupported_node.mtlx").expect("report");
    assert_eq!(body["compiles"], true);
    assert_eq!(body["unsupported"].as_array().map(Vec::len), Some(2));
    let missing = agent_report(&root, "assets/materials/missing.mtlx").expect_err("missing");
    assert_eq!(missing.0, "mtlx_io");
    let _ = std::fs::remove_dir_all(&root);
}

/// Seeded text mutation of every fixture through the whole import and an unvalidated compile: errors are
/// fine, panics and hangs are not.
#[test]
fn mutated_documents_never_panic() {
    for (k, name) in [
        "textured_standard_surface.mtlx",
        "nodegraph_mix_multiply.mtlx",
        "open_pbr_surface.mtlx",
        "custom_nodedef.mtlx",
    ]
    .into_iter()
    .enumerate()
    {
        let text = fixture(name);
        regia_test_utils::fuzz::fuzz_text_loader(&text, 0x3268 + k as u64, 300, |doc| {
            regia_asset_pipeline::fuzz_entry::materialx_load(doc)
        });
    }
}
