// SPDX-License-Identifier: MPL-2.0
//! Material coverage the base lint did not check (CT-09, ADR-8392).
//!
//! Texture indices, UV channels, slot color space, extension numeric domains
//! and importer capability. Unknown optional extensions stay byte-identical.
//! A required unsupported extension fails compatibility and is not stripped.

use std::path::Path;

use regia_asset_pipeline::gltf_material_lint::{
    MAX_EXTENSION_BYTES, MAX_EXTENSION_DEPTH, MaterialLintReport, MaterialLintSettings, importer_capability,
    lint_materials,
};
use regia_asset_pipeline::source_lint::{
    ProduceLimits, SourceDocument, SourceLintPolicy, SupportStatus, produce, rules,
};
use regia_asset_pipeline::{gltf_slot_color_space, usage_from_file_name};
use serde_json::{Value, json};

fn check(document: &Value) -> MaterialLintReport {
    let mut owned = document.clone();
    let report = lint_materials(&mut owned, &MaterialLintSettings::default(), false).expect("lint");
    assert_eq!(&owned, document, "check mode must not mutate");
    report
}

fn fix(document: &mut Value) -> MaterialLintReport {
    lint_materials(document, &MaterialLintSettings::default(), true).expect("fix")
}

fn codes(report: &MaterialLintReport) -> Vec<&str> {
    report
        .findings
        .iter()
        .map(|finding| finding.code.as_str())
        .collect()
}

fn has(report: &MaterialLintReport, code: &str) -> bool {
    report.findings.iter().any(|finding| finding.code == code)
        && report.remaining.iter().any(|finding| finding.code == code)
}

#[test]
fn bad_texture_index_is_a_stable_finding() {
    let document = json!({
        "asset": {"version": "2.0"},
        "images": [{"uri": "only.png"}],
        "textures": [{"source": 0}],
        "materials": [{
            "pbrMetallicRoughness": {"baseColorTexture": {"index": 4}}
        }]
    });
    let report = check(&document);
    let finding = report
        .findings
        .iter()
        .find(|finding| finding.code == "texture_index")
        .expect("texture_index");
    assert!(finding.pointer.contains("index"), "{}", finding.pointer);
    assert!(finding.message.contains("4"), "{}", finding.message);
    assert!(has(&report, "texture_index"));
    let mut fixed = document.clone();
    let repaired = fix(&mut fixed);
    assert_eq!(
        fixed["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["index"],
        4
    );
    assert!(
        repaired
            .remaining
            .iter()
            .any(|finding| finding.code == "texture_index")
    );

    let mut sources = document.clone();
    sources["textures"][0]["source"] = json!(3);
    let report = check(&sources);
    assert!(
        report
            .findings
            .iter()
            .any(|finding| { finding.code == "texture_index" && finding.pointer == "/textures/0/source" }),
        "{:?}",
        report.findings
    );

    let partial = json!({
        "asset": {"version": "2.0"},
        "materials": [{
            "pbrMetallicRoughness": {"metallicRoughnessTexture": {"index": 0}}
        }]
    });
    let report = check(&partial);
    assert!(
        !codes(&report).contains(&"texture_index"),
        "{:?}",
        report.findings
    );
}

#[test]
fn extension_numeric_domains_stay_unfixed() {
    let document = json!({
        "asset": {"version": "2.0"},
        "extensionsUsed": ["KHR_materials_transmission", "KHR_materials_volume", "KHR_materials_ior"],
        "materials": [{
            "extensions": {
                "KHR_materials_transmission": {"transmissionFactor": 1.0},
                "KHR_materials_volume": {"thicknessFactor": -0.2, "attenuationDistance": 0.0},
                "KHR_materials_ior": {"ior": 0.4}
            }
        }]
    });
    let report = check(&document);
    assert!(has(&report, "volume_thickness"), "{:?}", report.findings);
    assert!(has(&report, "attenuation_distance"), "{:?}", report.findings);
    assert!(has(&report, "ior_domain"), "{:?}", report.findings);
    let mut fixed = document.clone();
    let repaired = fix(&mut fixed);
    assert_eq!(
        fixed["materials"][0]["extensions"]["KHR_materials_volume"]["thicknessFactor"],
        -0.2
    );
    assert_eq!(
        fixed["materials"][0]["extensions"]["KHR_materials_volume"]["attenuationDistance"],
        0.0
    );
    assert_eq!(
        fixed["materials"][0]["extensions"]["KHR_materials_ior"]["ior"],
        0.4
    );
    for code in ["volume_thickness", "attenuation_distance", "ior_domain"] {
        assert!(
            repaired.remaining.iter().any(|finding| finding.code == code),
            "{code}"
        );
    }

    let valid = json!({
        "asset": {"version": "2.0"},
        "extensionsUsed": ["KHR_materials_transmission", "KHR_materials_volume", "KHR_materials_ior"],
        "materials": [{
            "extensions": {
                "KHR_materials_transmission": {"transmissionFactor": 1.0},
                "KHR_materials_volume": {"thicknessFactor": 0.0},
                "KHR_materials_ior": {"ior": 1.0}
            }
        }]
    });
    let report = check(&valid);
    for code in [
        "volume_thickness",
        "attenuation_distance",
        "ior_domain",
        "extension_dependency",
    ] {
        assert!(!codes(&report).contains(&code), "{code} {:?}", report.findings);
    }
    let mut ior = valid.clone();
    ior["materials"][0]["extensions"]["KHR_materials_ior"]["ior"] = json!(1.5);
    let report = check(&ior);
    assert!(!codes(&report).contains(&"ior_domain"));
}

#[test]
fn volume_without_transmission_is_not_added() {
    let document = json!({
        "asset": {"version": "2.0"},
        "extensionsUsed": ["KHR_materials_volume"],
        "materials": [{
            "extensions": {"KHR_materials_volume": {"thicknessFactor": 0.2}}
        }]
    });
    let report = check(&document);
    assert!(has(&report, "extension_dependency"), "{:?}", report.findings);
    let mut fixed = document.clone();
    let repaired = fix(&mut fixed);
    assert!(
        fixed["materials"][0]["extensions"]
            .get("KHR_materials_transmission")
            .is_none()
    );
    assert!(
        repaired
            .remaining
            .iter()
            .any(|finding| finding.code == "extension_dependency")
    );
}

#[test]
fn nested_texture_transform_declaration_is_completed() {
    let transform = json!({"offset": [0.1, 0.2], "texCoord": 0});
    let document = json!({
        "asset": {"version": "2.0"},
        "images": [{"uri": "color.png"}],
        "textures": [{"source": 0}],
        "materials": [{
            "pbrMetallicRoughness": {
                "baseColorTexture": {
                    "index": 0,
                    "extensions": {"KHR_texture_transform": transform}
                }
            }
        }]
    });
    let report = check(&document);
    assert!(
        report.findings.iter().any(|finding| {
            finding.code == "extensions_used" && finding.message.contains("KHR_texture_transform")
        }),
        "{:?}",
        report.findings
    );
    assert!(report.compatible);
    assert!(report.extensions.iter().any(|extension| {
        extension.name == "KHR_texture_transform" && extension.status == "side_table" && !extension.required
    }));
    let mut fixed = document.clone();
    let before = fixed["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["extensions"]["KHR_texture_transform"].clone();
    let repaired = fix(&mut fixed);
    assert_eq!(
        fixed["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["extensions"]["KHR_texture_transform"],
        before
    );
    let used = fixed["extensionsUsed"].as_array().expect("used");
    assert!(used.iter().any(|name| name == "KHR_texture_transform"));
    assert!(
        repaired
            .remaining
            .iter()
            .all(|finding| finding.code != "extensions_used")
    );
    assert!(!repaired.changed || fixed["extensionsUsed"] != document["extensionsUsed"]);
}

#[test]
fn unknown_optional_survives_and_required_unsupported_fails_compatibility() {
    let kept = json!({"z": 1, "a": [true, false]});
    let needed = json!({"keep": "yes"});
    let document = json!({
        "asset": {"version": "2.0"},
        "extensionsRequired": ["VENDOR_need"],
        "materials": [{
            "extensions": {"VENDOR_keep": kept, "VENDOR_need": needed}
        }]
    });
    let keep_bytes =
        serde_json::to_vec(&document["materials"][0]["extensions"]["VENDOR_keep"]).expect("bytes");
    let need_bytes =
        serde_json::to_vec(&document["materials"][0]["extensions"]["VENDOR_need"]).expect("bytes");
    let report = check(&document);
    assert!(!report.compatible);
    assert!(
        report
            .extensions
            .iter()
            .any(|row| { row.name == "VENDOR_keep" && !row.required && row.status == "unsupported" })
    );
    assert!(
        report
            .extensions
            .iter()
            .any(|row| { row.name == "VENDOR_need" && row.required && row.status == "unsupported" })
    );
    assert!(
        report
            .findings
            .iter()
            .all(|finding| finding.code != "extension_nesting")
    );
    let mut fixed = document.clone();
    let repaired = fix(&mut fixed);
    assert_eq!(
        serde_json::to_vec(&fixed["materials"][0]["extensions"]["VENDOR_keep"]).expect("bytes"),
        keep_bytes
    );
    assert_eq!(
        serde_json::to_vec(&fixed["materials"][0]["extensions"]["VENDOR_need"]).expect("bytes"),
        need_bytes
    );
    assert_eq!(fixed["extensionsRequired"], json!(["VENDOR_need"]));
    assert!(!repaired.compatible);
    assert!(
        repaired
            .remaining
            .iter()
            .all(|finding| finding.code != "extension_nesting")
    );
}

#[test]
fn slot_color_space_follows_usage_not_file_name() {
    assert_eq!(gltf_slot_color_space("baseColorTexture"), "srgb");
    assert_eq!(gltf_slot_color_space("emissiveTexture"), "srgb");
    assert_eq!(gltf_slot_color_space("metallicRoughnessTexture"), "linear");
    assert_eq!(gltf_slot_color_space("occlusionTexture"), "linear");
    assert_eq!(gltf_slot_color_space("normalTexture"), "normal");
    assert_ne!(
        usage_from_file_name(Path::new("metal_n.png")),
        usage_from_file_name(Path::new("albedo.png"))
    );
    let document = json!({
        "asset": {"version": "2.0"},
        "images": [{"uri": "metal_n.png"}, {"uri": "color_orm.png"}],
        "textures": [{"source": 0}, {"source": 1}],
        "materials": [{
            "pbrMetallicRoughness": {
                "baseColorTexture": {"index": 0},
                "metallicRoughnessTexture": {"index": 1}
            }
        }]
    });
    let report = check(&document);
    assert!(
        !codes(&report).contains(&"texture_color_space"),
        "{:?}",
        report.findings
    );
    let base = report
        .texture_roles
        .iter()
        .find(|role| role.role == "base_color")
        .expect("base");
    let data = report
        .texture_roles
        .iter()
        .find(|role| role.role == "metallic_roughness")
        .expect("data");
    assert_eq!(base.color_space, "srgb");
    assert_eq!(data.color_space, "linear");
    assert_eq!(base.texture, Some(0));
    assert_eq!(data.texture, Some(1));

    let mut shared = document.clone();
    shared["materials"][0]["pbrMetallicRoughness"]["metallicRoughnessTexture"]["index"] = json!(0);
    shared["textures"] = json!([{"source": 0}]);
    shared["images"] = json!([{"uri": "metal_n.png"}]);
    let report = check(&shared);
    assert!(has(&report, "texture_color_space"), "{:?}", report.findings);
    let mut fixed = shared.clone();
    let repaired = fix(&mut fixed);
    assert_eq!(
        fixed["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["index"],
        0
    );
    assert!(
        repaired
            .remaining
            .iter()
            .any(|finding| finding.code == "texture_color_space")
    );
}

#[test]
fn material_referencing_uv1_without_uv1_is_localized() {
    let document = json!({
        "asset": {"version": "2.0"},
        "images": [{"uri": "a.png"}],
        "textures": [{"source": 0}],
        "materials": [{
            "pbrMetallicRoughness": {"baseColorTexture": {"index": 0, "texCoord": 1}}
        }],
        "meshes": [{
            "primitives": [{
                "attributes": {"POSITION": 0, "TEXCOORD_0": 1},
                "material": 0
            }]
        }],
        "accessors": [
            {"componentType": 5126, "count": 3, "type": "VEC3"},
            {"componentType": 5126, "count": 3, "type": "VEC2"}
        ]
    });
    let report = check(&document);
    let finding = report
        .findings
        .iter()
        .find(|finding| finding.code == "uv_channel")
        .expect("uv");
    assert!(finding.pointer.contains("texCoord"), "{}", finding.pointer);
    assert!(finding.message.contains("mesh 0"), "{}", finding.message);
    assert!(finding.message.contains("primitive 0"), "{}", finding.message);
    assert!(finding.message.contains("TEXCOORD_1"), "{}", finding.message);
    let mut fixed = document.clone();
    let repaired = fix(&mut fixed);
    assert_eq!(
        fixed["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["texCoord"],
        1
    );
    assert!(
        repaired
            .remaining
            .iter()
            .any(|finding| finding.code == "uv_channel")
    );

    let mut present = document.clone();
    present["meshes"][0]["primitives"][0]["attributes"]["TEXCOORD_1"] = json!(1);
    let report = check(&present);
    assert!(!codes(&report).contains(&"uv_channel"), "{:?}", report.findings);

    let mut fractional = document.clone();
    fractional["meshes"] = json!([]);
    fractional["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["texCoord"] = json!(-1);
    let before = fractional.clone();
    let report = check(&fractional);
    assert!(has(&report, "uv_channel"));
    assert_eq!(fractional, before);

    let mut wrong_type = document;
    wrong_type["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["texCoord"] = json!("second");
    let mut owned = wrong_type.clone();
    let error = lint_materials(&mut owned, &MaterialLintSettings::default(), true).expect_err("type");
    assert_eq!(error.code, "gltf_material_invalid");
    assert_eq!(owned, wrong_type);
}

#[test]
fn nested_extension_json_is_bounded() {
    let mut deep = json!({"leaf": true});
    for _ in 0..40 {
        deep = json!({"k": deep});
    }
    let mut document = json!({
        "asset": {"version": "2.0"},
        "materials": [{"extensions": {"VENDOR_nest": deep}}]
    });
    let payload = document["materials"][0]["extensions"]["VENDOR_nest"].clone();
    let first = check(&document);
    let second = check(&document);
    assert!(has(&first, "extension_nesting"), "{:?}", first.findings);
    assert_eq!(codes(&first), codes(&second));
    assert_eq!(first.extension_nodes, second.extension_nodes);
    assert_eq!(first.extension_bytes, second.extension_bytes);
    assert!(first.extension_nodes <= u64::try_from(MAX_EXTENSION_DEPTH).unwrap_or(u64::MAX) + 1);
    assert!(first.extension_bytes <= MAX_EXTENSION_BYTES);
    assert!(first.extension_nodes > 0);
    let repaired = fix(&mut document);
    assert_eq!(document["materials"][0]["extensions"]["VENDOR_nest"], payload);
    assert!(
        repaired
            .remaining
            .iter()
            .any(|finding| finding.code == "extension_nesting")
    );

    let huge = "x".repeat(usize::try_from(MAX_EXTENSION_BYTES).unwrap_or(usize::MAX) + 8);
    let mut wide = json!({
        "asset": {"version": "2.0"},
        "materials": [{"extensions": {"VENDOR_blob": {"payload": huge}}}]
    });
    let blob = wide["materials"][0]["extensions"]["VENDOR_blob"].clone();
    let report = check(&wide);
    let again = check(&wide);
    assert!(has(&report, "extension_nesting"), "{:?}", report.findings);
    assert_eq!(report.extension_bytes, MAX_EXTENSION_BYTES);
    assert_eq!(report.extension_bytes, again.extension_bytes);
    assert_eq!(report.extension_nodes, again.extension_nodes);
    fix(&mut wide);
    assert_eq!(wide["materials"][0]["extensions"]["VENDOR_blob"], blob);

    let shallow = json!({
        "asset": {"version": "2.0"},
        "materials": [{"extensions": {"VENDOR_keep": {"a": {"b": {"c": 1}}}}}]
    });
    let report = check(&shallow);
    assert!(
        !codes(&report).contains(&"extension_nesting"),
        "{:?}",
        report.findings
    );
    assert!(
        report
            .extensions
            .iter()
            .any(|row| row.name == "VENDOR_keep" && row.status == "unsupported")
    );
    assert!(report.extension_nodes <= u64::try_from(MAX_EXTENSION_DEPTH).unwrap_or(u64::MAX));
    assert!(report.extension_bytes <= MAX_EXTENSION_BYTES);
}

#[test]
fn rule_catalog_and_importer_capability_match() {
    let ids: Vec<&str> = rules().iter().map(|rule| rule.id).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted);
    // 25 with material coverage, plus the five geometry-complete rules (ADR-8347) and the five
    // texture-lint rules (ADR-8536).
    assert_eq!(ids.len(), 35);
    for id in [
        "attenuation_distance",
        "emissive_strength",
        "extension_dependency",
        "extension_nesting",
        "extensions_used",
        "factor_missing",
        "ior_domain",
        "orm_factors",
        "texture_color_space",
        "texture_index",
        "transmission_ior",
        "uv_channel",
        "volume_thickness",
    ] {
        assert!(ids.contains(&id), "{id}");
    }
    let capability = importer_capability();
    assert!(
        capability
            .iter()
            .any(|row| row.name == "KHR_materials_volume" && row.status == "cooked")
    );
    assert!(
        capability
            .iter()
            .any(|row| row.name == "KHR_texture_transform" && row.status == "side_table")
    );
    assert!(capability.iter().all(|row| row.name != "VENDOR_keep"));
}

#[test]
fn required_unsupported_extension_fails_source_support_without_a_stripping_finding() {
    let bytes = br#"{"asset":{"version":"2.0"},"extensionsUsed":["VENDOR_need"],"extensionsRequired":["VENDOR_need"],"materials":[{"extensions":{"VENDOR_need":{"keep":"yes"}}}]}"#;
    let limits = ProduceLimits {
        max_assets: 4,
        max_findings: 32,
        max_bytes: 1024 * 1024,
    };
    let report = produce(
        &[SourceDocument {
            path: "vendor.gltf",
            bytes,
        }],
        &SourceLintPolicy::engine_defaults(),
        "default",
        &limits,
        &|| false,
    )
    .expect("produce");
    assert_eq!(report.sources[0].support, SupportStatus::Unsupported);
    assert_eq!(report.sources[0].reason.as_deref(), Some("gltf_ext_unsupported"));
    assert!(
        report
            .findings
            .iter()
            .all(|finding| finding.rule_id != "extension_nesting")
    );
    assert_eq!(report.work.rules_checked, rules().len());

    let optional = br#"{"asset":{"version":"2.0"},"extensionsUsed":["VENDOR_keep"],"materials":[{"extensions":{"VENDOR_keep":{"z":1}}}]}"#;
    let report = produce(
        &[SourceDocument {
            path: "optional.gltf",
            bytes: optional,
        }],
        &SourceLintPolicy::engine_defaults(),
        "default",
        &limits,
        &|| false,
    )
    .expect("produce");
    assert_eq!(report.sources[0].support, SupportStatus::Supported);
    assert!(report.sources[0].reason.is_none());
}
