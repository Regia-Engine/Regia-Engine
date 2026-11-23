// SPDX-License-Identifier: MPL-2.0
//! Material authoring regressions (C-05, ADR-8061).
use regia_asset_pipeline::gltf_material_lint::{MaterialLintProfile, MaterialLintSettings, lint_materials};
use serde_json::json;

fn compatibility() -> MaterialLintSettings {
    MaterialLintSettings {
        profile: MaterialLintProfile::Compatibility,
        ..Default::default()
    }
}

#[test]
fn orm_with_metallic_1_flagged_and_fixed() {
    let mut document = json!({"asset":{"version":"2.0"},"materials":[{
        "pbrMetallicRoughness":{"metallicRoughnessTexture":{"index":0},
            "metallicFactor":1.0,"roughnessFactor":0.3}
    }]});
    let settings = compatibility();
    let before = document.clone();
    let report = lint_materials(&mut document, &settings, false).expect("lint");
    assert!(report.findings.iter().any(|f| f.code == "orm_factors"));
    assert_eq!(document, before, "check mode must not mutate");
    let report = lint_materials(&mut document, &settings, true).expect("fix");
    assert!(report.changed);
    assert!(report.remaining.is_empty());
    assert_eq!(
        document["materials"][0]["pbrMetallicRoughness"]["roughnessFactor"],
        1.0
    );
    assert_eq!(
        document["materials"][0]["pbrMetallicRoughness"]["metallicFactor"],
        1.0
    );
    assert!(
        !lint_materials(&mut document, &settings, true)
            .expect("again")
            .changed
    );
}

fn document(material: serde_json::Value) -> serde_json::Value {
    json!({"asset":{"version":"2.0"},"materials":[material]})
}

fn fix(g: &mut serde_json::Value) {
    let settings = MaterialLintSettings::default();
    let report = lint_materials(g, &settings, true).expect("repair");
    assert!(report.remaining.is_empty(), "{:?}", report.remaining);
    assert!(!lint_materials(g, &settings, true).expect("idempotent").changed);
}

#[test]
fn zero_metallic_cannot_cancel_the_orm_channel() {
    let mut g = document(json!({"pbrMetallicRoughness":{
        "metallicRoughnessTexture":{"index":0},"metallicFactor":0.0,"roughnessFactor":0.0}}));
    let settings = compatibility();
    let report = lint_materials(&mut g, &settings, true).expect("repair");
    assert!(report.remaining.is_empty(), "{:?}", report.remaining);
    assert!(
        !lint_materials(&mut g, &settings, true)
            .expect("idempotent")
            .changed
    );
    assert_eq!(g["materials"][0]["pbrMetallicRoughness"]["metallicFactor"], 1.0);
    assert_eq!(g["materials"][0]["pbrMetallicRoughness"]["roughnessFactor"], 1.0);
}

#[test]
fn valid_orm_is_not_classified_by_its_name() {
    let mut g = document(json!({"name":"paint", "pbrMetallicRoughness":{
        "metallicRoughnessTexture":{"index":0},"metallicFactor":1.0,"roughnessFactor":1.0}}));
    let settings = serde_json::from_value(json!({"name_rules":[{"contains":"paint","metallic":0.0}]}))
        .expect("settings");
    let report = lint_materials(&mut g, &settings, true).expect("lint");
    assert!(report.findings.is_empty());
    assert!(!report.changed);
}

#[test]
fn factor_bounds_and_missing_factors() {
    let mut g = document(json!({"pbrMetallicRoughness":{
        "metallicFactor":-0.2,"roughnessFactor":4.0,"baseColorFactor":[-1,2,0.5,3]}}));
    fix(&mut g);
    assert_eq!(
        g["materials"][0]["pbrMetallicRoughness"],
        json!({
        "metallicFactor":0.0,"roughnessFactor":1.0,"baseColorFactor":[0.0,1.0,0.5,1.0]})
    );
    let mut g = document(json!({}));
    let settings = compatibility();
    let report = lint_materials(&mut g, &settings, true).expect("repair");
    assert!(report.remaining.is_empty(), "{:?}", report.remaining);
    assert!(
        !lint_materials(&mut g, &settings, true)
            .expect("idempotent")
            .changed
    );
    assert_eq!(
        g["materials"][0]["pbrMetallicRoughness"],
        json!({"metallicFactor":0.0,"roughnessFactor":1.0})
    );
    assert!(g["materials"][0].get("extensions").is_none());
}

#[test]
fn name_policy_is_explicit_case_insensitive_and_first_match_wins() {
    let settings = serde_json::from_value(json!({"name_rules":[
        {"contains":"coated","metallic":0.0,"roughness":0.8},
        {"contains":"metal","metallic":1.0,"roughness":0.4}]}))
    .expect("policy");
    let mut g = document(json!({"name":"COATED metal", "pbrMetallicRoughness":{"metallicFactor":1.0}}));
    let report = lint_materials(&mut g, &settings, true).expect("fix");
    assert!(report.findings.iter().any(|f| f.code == "metallic_name_policy"));
    assert_eq!(g["materials"][0]["pbrMetallicRoughness"]["metallicFactor"], 0.0);
    assert_eq!(g["materials"][0]["pbrMetallicRoughness"]["roughnessFactor"], 0.8);
    let mut bare = document(json!({"pbrMetallicRoughness":{"metallicFactor":1.0,"roughnessFactor":0.4}}));
    fix(&mut bare);
    assert_eq!(
        bare["materials"][0]["pbrMetallicRoughness"]["metallicFactor"],
        1.0
    );
}

#[test]
fn hdr_emission_preserves_energy_and_clamps_negative_channels() {
    let mut g = document(json!({"emissiveFactor":[4.0,2.0,-1.0],
        "extensions":{"KHR_materials_emissive_strength":{"emissiveStrength":3.0}}}));
    fix(&mut g);
    assert_eq!(g["materials"][0]["emissiveFactor"], json!([1.0, 0.5, 0.0]));
    assert_eq!(
        g["materials"][0]["extensions"]["KHR_materials_emissive_strength"]["emissiveStrength"],
        12.0
    );
    assert!(
        g["extensionsUsed"]
            .as_array()
            .expect("used")
            .contains(&json!("KHR_materials_emissive_strength"))
    );
}

#[test]
fn emissive_texture_gets_strength_and_nonemissive_stays_dark() {
    let mut g = document(json!({"emissiveTexture":{"index":0}}));
    fix(&mut g);
    assert_eq!(
        g["materials"][0]["extensions"]["KHR_materials_emissive_strength"]["emissiveStrength"],
        1.0
    );
    let mut dark = document(json!({"emissiveFactor":[-1,0,0]}));
    fix(&mut dark);
    assert_eq!(dark["materials"][0]["emissiveFactor"], json!([0.0, 0.0, 0.0]));
    assert!(dark["materials"][0].get("extensions").is_none());
}

#[test]
fn named_emissive_minimum_is_configurable() {
    let s = serde_json::from_value(json!({"name_rules":[{"contains":"lamp","emissive_strength":42.0}]}))
        .expect("settings");
    let mut g = document(json!({"name":"Lamp", "emissiveFactor":[0.5,0.2,0.1]}));
    let report = lint_materials(&mut g, &s, true).expect("fix");
    assert!(report.remaining.is_empty());
    assert_eq!(
        g["materials"][0]["extensions"]["KHR_materials_emissive_strength"]["emissiveStrength"],
        42.0
    );
}

#[test]
fn volume_tint_moves_but_thin_pane_tint_and_alpha_survive() {
    for volume in [false, true] {
        let mut m = json!({"pbrMetallicRoughness":{"baseColorFactor":[0.2,0.5,0.8,0.3]},
            "extensions":{"KHR_materials_transmission":{"transmissionFactor":1.0}}});
        if volume {
            m["extensions"]["KHR_materials_volume"] = json!({"thicknessFactor":0.4});
        }
        let mut g = document(m);
        let settings = if volume {
            compatibility()
        } else {
            MaterialLintSettings::default()
        };
        let report = lint_materials(&mut g, &settings, true).expect("repair");
        assert!(report.remaining.is_empty(), "{:?}", report.remaining);
        assert!(
            !lint_materials(&mut g, &settings, true)
                .expect("idempotent")
                .changed
        );
        let m = &g["materials"][0];
        assert_eq!(
            m["pbrMetallicRoughness"]["baseColorFactor"],
            if volume {
                json!([1.0, 1.0, 1.0, 0.3])
            } else {
                json!([0.2, 0.5, 0.8, 0.3])
            }
        );
        assert_eq!(m["extensions"]["KHR_materials_ior"]["ior"], 1.5);
        if volume {
            assert_eq!(
                m["extensions"]["KHR_materials_volume"]["attenuationColor"],
                json!([0.2, 0.5, 0.8])
            );
            assert_eq!(m["extensions"]["KHR_materials_volume"]["thicknessFactor"], 0.4);
        } else {
            assert!(m["extensions"].get("KHR_materials_volume").is_none());
        }
    }
}

#[test]
fn authored_volume_attenuation_ior_and_unknown_data_survive() {
    let mut g = document(
        json!({"extras":{"note":"retain"},"pbrMetallicRoughness":{"baseColorFactor":[0.2,0.3,0.4,1]},
        "extensions":{"CUSTOM_material":{"field":17},"KHR_materials_transmission":{},
        "KHR_materials_volume":{"attenuationColor":[0.5,0.6,0.7]},"KHR_materials_ior":{"ior":1.8}}}),
    );
    g["extensionsUsed"] = json!(["KHR_texture_transform"]);
    g["extensionsRequired"] = json!(["CUSTOM_material"]);
    g["meshes"] = json!([{"unknown":"opaque payload"}]);
    fix(&mut g);
    assert_eq!(
        g["materials"][0]["extensions"]["KHR_materials_volume"]["attenuationColor"],
        json!([0.5, 0.6, 0.7])
    );
    assert_eq!(g["materials"][0]["extensions"]["KHR_materials_ior"]["ior"], 1.8);
    assert_eq!(g["materials"][0]["extensions"]["CUSTOM_material"]["field"], 17);
    assert_eq!(g["materials"][0]["extras"]["note"], "retain");
    assert_eq!(g["meshes"], json!([{"unknown":"opaque payload"}]));
    assert_eq!(g["extensionsUsed"][0], "KHR_texture_transform");
    assert_eq!(g["extensionsRequired"], json!(["CUSTOM_material"]));
}

#[test]
fn malformed_documents_are_errors_and_never_partially_fixed() {
    for m in [
        json!(null),
        json!({"pbrMetallicRoughness":null}),
        json!({"pbrMetallicRoughness":{"metallicFactor":"bad"}}),
        json!({"pbrMetallicRoughness":{"baseColorFactor":[1,2,3]}}),
        json!({"emissiveFactor":[1,true,0]}),
        json!({"extensions":[]}),
        json!({"extensions":{"KHR_materials_transmission":null}}),
        json!({"emissiveTexture":{"index":-1}}),
    ] {
        let mut g = json!({"asset":{"version":"2.0"},"materials":[{},m]});
        let before = g.clone();
        assert_eq!(
            lint_materials(&mut g, &MaterialLintSettings::default(), true)
                .expect_err("malformed")
                .code,
            "gltf_material_invalid"
        );
        assert_eq!(g, before);
    }
}

#[test]
fn bad_settings_and_emissive_overflow_never_modify_input() {
    let mut g = document(json!({"emissiveFactor":[1e308,0,0],
        "extensions":{"KHR_materials_emissive_strength":{"emissiveStrength":1e308}}}));
    let before = g.clone();
    assert!(lint_materials(&mut g, &MaterialLintSettings::default(), true).is_err());
    assert_eq!(g, before);
    let settings = MaterialLintSettings {
        default_metallic: f64::NAN,
        ..Default::default()
    };
    assert_eq!(
        lint_materials(&mut g, &settings, true)
            .expect_err("settings")
            .code,
        "gltf_material_settings"
    );
}

#[test]
fn default_dielectric_sweep_is_opt_in_and_keeps_named_bare_metal() {
    let mut g = json!({"asset":{"version":"2.0"},"materials":[
        {"name":"Unclassified panel","pbrMetallicRoughness":{"metallicFactor":1.0,"roughnessFactor":0.5}},
        {"name":"Bare metal","pbrMetallicRoughness":{"metallicFactor":1.0,"roughnessFactor":0.5}},
        {"pbrMetallicRoughness":{"metallicFactor":1.0,"roughnessFactor":0.5}}
    ]});
    assert!(
        !lint_materials(&mut g, &MaterialLintSettings::default(), true)
            .expect("opt out")
            .changed
    );
    let settings = serde_json::from_value(json!({"classify_untextured":true,
        "name_rules":[{"contains":"bare metal","metallic":1.0}]}))
    .expect("policy");
    let report = lint_materials(&mut g, &settings, true).expect("sweep");
    assert_eq!(
        report
            .findings
            .iter()
            .filter(|f| f.code == "metallic_name_policy")
            .count(),
        2
    );
    assert_eq!(g["materials"][0]["pbrMetallicRoughness"]["metallicFactor"], 0.0);
    assert_eq!(g["materials"][1]["pbrMetallicRoughness"]["metallicFactor"], 1.0);
    assert_eq!(g["materials"][2]["pbrMetallicRoughness"]["metallicFactor"], 0.0);
    assert!(report.remaining.is_empty());
}

#[test]
fn malformed_document_roots_and_extension_lists_are_refused() {
    for mut g in [
        json!(null),
        json!([]),
        json!({}),
        json!({"asset":{"version":"1.0"}}),
        json!({"asset":{"version":"2.0"},"materials":null}),
        json!({"asset":{"version":"2.0"},"extensionsUsed":[12]}),
        json!({"asset":{"version":"2.0"},"extensionsRequired":"KHR_materials_volume"}),
    ] {
        let before = g.clone();
        assert_eq!(
            lint_materials(&mut g, &MaterialLintSettings::default(), true)
                .expect_err("invalid")
                .code,
            "gltf_material_invalid"
        );
        assert_eq!(g, before);
    }
}

#[test]
fn configured_volume_dimensions_fill_missing_values_without_overriding_authored_ones() {
    let settings = MaterialLintSettings {
        profile: MaterialLintProfile::Compatibility,
        default_volume_thickness: Some(0.4),
        default_attenuation_distance: Some(0.7),
        ..Default::default()
    };
    for authored in [false, true] {
        let mut g = document(
            json!({"pbrMetallicRoughness":{"baseColorFactor":[0.5,0.6,0.7,1.0]},
            "extensions":{"KHR_materials_transmission":{},"KHR_materials_volume":{}}}),
        );
        if authored {
            g["materials"][0]["extensions"]["KHR_materials_volume"] =
                json!({"thicknessFactor":0.8,"attenuationDistance":1.2});
        }
        let report = lint_materials(&mut g, &settings, true).expect("fix");
        assert!(report.remaining.is_empty());
        let volume = &g["materials"][0]["extensions"]["KHR_materials_volume"];
        assert_eq!(volume["thicknessFactor"], if authored { 0.8 } else { 0.4 });
        assert_eq!(volume["attenuationDistance"], if authored { 1.2 } else { 0.7 });
    }
}
