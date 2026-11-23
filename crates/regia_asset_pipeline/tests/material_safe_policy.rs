// SPDX-License-Identifier: MPL-2.0
//! Default material repair preserves valid glTF appearance (CT-07, ADR-8250).
//!
//! On the released base these default-profile checks fail: a textured factor
//! product is reset to 1, a missing metallicFactor is written as 0, and a
//! colored base is replaced when volume attenuation exists. The compatibility
//! profile keeps the original policy assertions.
use regia_asset_pipeline::gltf_material_lint::{
    EMISSIVE_PRODUCT_REL_TOLERANCE, MAX_FINDINGS_PER_MATERIAL, MaterialLintProfile, MaterialLintReport,
    MaterialLintSettings, MaterialNameRule, MaterialRepairEffect, lint_materials,
};
use serde_json::{Value, json};

fn document(material: Value) -> Value {
    json!({"asset":{"version":"2.0"},"materials":[material]})
}

fn compatibility() -> MaterialLintSettings {
    MaterialLintSettings {
        profile: MaterialLintProfile::Compatibility,
        ..Default::default()
    }
}

fn metallic_semantic(material: &Value) -> f64 {
    material
        .pointer("/pbrMetallicRoughness/metallicFactor")
        .and_then(Value::as_f64)
        .unwrap_or(1.0)
}

fn fix_twice(g: &mut Value, settings: &MaterialLintSettings) -> MaterialLintReport {
    let report = lint_materials(g, settings, true).expect("repair");
    assert!(report.remaining.is_empty(), "{:?}", report.remaining);
    let again = lint_materials(g, settings, true).expect("second fix");
    assert!(!again.changed, "a second fix must be idempotent");
    assert!(again.remaining.is_empty(), "{:?}", again.remaining);
    report
}

#[test]
fn valid_textured_factors_stay_under_the_default_fix() {
    let mut g = document(json!({"pbrMetallicRoughness":{
        "metallicRoughnessTexture":{"index":0},
        "metallicFactor":0.35,
        "roughnessFactor":0.7
    }}));
    let before = g.clone();
    let settings = MaterialLintSettings::default();
    let report = lint_materials(&mut g, &settings, false).expect("check");
    assert_eq!(report.profile, MaterialLintProfile::Standard);
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert!(!report.changed);
    assert_eq!(g, before, "check mode must not mutate");
    let report = fix_twice(&mut g, &settings);
    assert!(!report.changed);
    assert_eq!(g, before);
    assert_eq!(g["materials"][0]["pbrMetallicRoughness"]["metallicFactor"], 0.35);
    assert_eq!(g["materials"][0]["pbrMetallicRoughness"]["roughnessFactor"], 0.7);
}

#[test]
fn missing_metallic_keeps_the_spec_default_of_one() {
    let settings = MaterialLintSettings::default();
    let mut bare = document(json!({"name":"panel"}));
    let before = bare.clone();
    let report = fix_twice(&mut bare, &settings);
    assert!(!report.changed);
    assert_eq!(bare, before);
    assert_eq!(metallic_semantic(&bare["materials"][0]), 1.0);

    let mut partial = document(json!({"pbrMetallicRoughness":{
        "roughnessFactor":0.4,"baseColorFactor":[0.2,0.3,0.4,1.0]
    }}));
    let report = fix_twice(&mut partial, &settings);
    assert!(!report.changed);
    assert!(
        partial["materials"][0]["pbrMetallicRoughness"]
            .get("metallicFactor")
            .is_none()
    );
    assert_eq!(metallic_semantic(&partial["materials"][0]), 1.0);
    assert_eq!(
        partial["materials"][0]["pbrMetallicRoughness"]["roughnessFactor"],
        0.4
    );
    assert_eq!(
        partial["materials"][0]["pbrMetallicRoughness"]["baseColorFactor"],
        json!([0.2, 0.3, 0.4, 1.0])
    );
}

#[test]
fn colored_base_and_authored_attenuation_both_stay() {
    let settings = MaterialLintSettings::default();
    let mut g = json!({"asset":{"version":"2.0"},
    "extensionsUsed":["KHR_materials_ior","KHR_materials_transmission","KHR_materials_volume"],
    "materials":[{
        "pbrMetallicRoughness":{"baseColorFactor":[0.2,0.4,0.6,1.0],"metallicFactor":0.0,"roughnessFactor":0.4},
        "extensions":{
            "KHR_materials_transmission":{"transmissionFactor":1.0},
            "KHR_materials_volume":{"thicknessFactor":0.5,"attenuationColor":[0.1,0.2,0.3],"attenuationDistance":1.2},
            "KHR_materials_ior":{"ior":1.45}
        }
    }]});
    let before = g.clone();
    let report = fix_twice(&mut g, &settings);
    assert!(!report.changed);
    assert_eq!(g, before);
    assert_eq!(
        g["materials"][0]["pbrMetallicRoughness"]["baseColorFactor"],
        json!([0.2, 0.4, 0.6, 1.0])
    );
    assert_eq!(
        g["materials"][0]["extensions"]["KHR_materials_volume"]["attenuationColor"],
        json!([0.1, 0.2, 0.3])
    );

    g["materials"][0]["extensions"]["KHR_materials_volume"] = json!({"thicknessFactor":0.5});
    let before = g.clone();
    let report = fix_twice(&mut g, &settings);
    assert!(!report.changed);
    assert_eq!(g, before);
    assert!(
        g["materials"][0]["extensions"]["KHR_materials_volume"]
            .get("attenuationColor")
            .is_none()
    );
}

#[test]
fn zero_thickness_volume_stays_a_thin_surface() {
    let settings = MaterialLintSettings::default();
    for volume in [json!({"thicknessFactor":0.0}), json!({})] {
        let mut g = json!({"asset":{"version":"2.0"},
        "extensionsUsed":["KHR_materials_ior","KHR_materials_transmission","KHR_materials_volume"],
        "materials":[{
            "pbrMetallicRoughness":{"baseColorFactor":[0.2,0.5,0.8,0.3],"metallicFactor":0.0,"roughnessFactor":0.2},
            "extensions":{
                "KHR_materials_transmission":{"transmissionFactor":1.0},
                "KHR_materials_volume":volume,
                "KHR_materials_ior":{"ior":1.5}
            }
        }]});
        let before = g.clone();
        let report = fix_twice(&mut g, &settings);
        assert!(!report.changed, "{volume}");
        assert_eq!(g, before, "{volume}");
        assert_eq!(
            g["materials"][0]["pbrMetallicRoughness"]["baseColorFactor"],
            json!([0.2, 0.5, 0.8, 0.3])
        );
        assert!(
            g["materials"][0]["extensions"]["KHR_materials_volume"]
                .get("attenuationColor")
                .is_none()
        );
    }
}

#[test]
fn format_clamps_stay_on_the_default_profile_and_do_not_touch_valid_factors() {
    let mut g = json!({"asset":{"version":"2.0"},"materials":[
        {"pbrMetallicRoughness":{"metallicFactor":1.4,"roughnessFactor":-0.2,"baseColorFactor":[-1.0,0.2,2.0,0.5]}},
        {"pbrMetallicRoughness":{"metallicRoughnessTexture":{"index":0},"metallicFactor":0.35,"roughnessFactor":0.7}}
    ]});
    let report = fix_twice(&mut g, &MaterialLintSettings::default());
    assert!(report.changed);
    assert!(
        report
            .findings
            .iter()
            .all(|f| f.effect == MaterialRepairEffect::PreservesAppearance)
    );
    assert_eq!(g["materials"][0]["pbrMetallicRoughness"]["metallicFactor"], 1.0);
    assert_eq!(g["materials"][0]["pbrMetallicRoughness"]["roughnessFactor"], 0.0);
    assert_eq!(
        g["materials"][0]["pbrMetallicRoughness"]["baseColorFactor"],
        json!([0.0, 0.2, 1.0, 0.5])
    );
    assert_eq!(g["materials"][1]["pbrMetallicRoughness"]["metallicFactor"], 0.35);
    assert_eq!(g["materials"][1]["pbrMetallicRoughness"]["roughnessFactor"], 0.7);
}

#[test]
fn explicit_name_rule_still_changes_appearance_without_the_compatibility_profile() {
    let settings: MaterialLintSettings = serde_json::from_value(json!({"name_rules":[
        {"contains":"coated","metallic":0.0,"roughness":0.8}
    ]}))
    .expect("policy");
    assert_eq!(settings.profile, MaterialLintProfile::Standard);
    let mut g = document(json!({"name":"Coated panel","pbrMetallicRoughness":{"metallicFactor":1.0}}));
    let report = fix_twice(&mut g, &settings);
    assert!(report.changed);
    assert!(report.findings.iter().any(|f| {
        f.code == "metallic_name_policy" && f.effect == MaterialRepairEffect::ChangesAppearance
    }));
    assert_eq!(g["materials"][0]["pbrMetallicRoughness"]["metallicFactor"], 0.0);
    assert_eq!(g["materials"][0]["pbrMetallicRoughness"]["roughnessFactor"], 0.8);
}

#[test]
fn compatibility_profile_keeps_the_original_policy_assertions() {
    let settings = compatibility();
    let mut orm_doc = json!({"asset":{"version":"2.0"},"materials":[{
        "pbrMetallicRoughness":{"metallicRoughnessTexture":{"index":0},
            "metallicFactor":1.0,"roughnessFactor":0.3}
    }]});
    let before = orm_doc.clone();
    let report = lint_materials(&mut orm_doc, &settings, false).expect("check");
    assert_eq!(report.profile, MaterialLintProfile::Compatibility);
    assert!(report.profile.changes_appearance());
    let orm = report
        .findings
        .iter()
        .find(|f| f.code == "orm_factors")
        .expect("orm finding");
    assert_eq!(orm.effect, MaterialRepairEffect::ChangesAppearance);
    assert_eq!(orm_doc, before, "check mode must not mutate");
    let report = fix_twice(&mut orm_doc, &settings);
    assert!(report.changed);
    assert_eq!(
        orm_doc["materials"][0]["pbrMetallicRoughness"]["roughnessFactor"],
        1.0
    );
    assert_eq!(
        orm_doc["materials"][0]["pbrMetallicRoughness"]["metallicFactor"],
        1.0
    );

    let mut cancelled = document(json!({"pbrMetallicRoughness":{
        "metallicRoughnessTexture":{"index":0},"metallicFactor":0.0,"roughnessFactor":0.0}}));
    fix_twice(&mut cancelled, &settings);
    assert_eq!(
        cancelled["materials"][0]["pbrMetallicRoughness"]["metallicFactor"],
        1.0
    );
    assert_eq!(
        cancelled["materials"][0]["pbrMetallicRoughness"]["roughnessFactor"],
        1.0
    );

    let mut missing = document(json!({}));
    let filled = lint_materials(&mut missing, &settings, true).expect("fill");
    assert!(filled.remaining.is_empty(), "{:?}", filled.remaining);
    assert_eq!(
        missing["materials"][0]["pbrMetallicRoughness"],
        json!({"metallicFactor":0.0,"roughnessFactor":1.0})
    );
    assert!(missing["materials"][0].get("extensions").is_none());
    assert!(filled.findings.iter().any(|f| {
        f.code == "factor_missing"
            && f.pointer.ends_with("metallicFactor")
            && f.effect == MaterialRepairEffect::ChangesAppearance
    }));
    assert!(
        !lint_materials(&mut missing, &settings, true)
            .expect("idempotent")
            .changed
    );

    for volume in [false, true] {
        let mut m = json!({"pbrMetallicRoughness":{"baseColorFactor":[0.2,0.5,0.8,0.3]},
            "extensions":{"KHR_materials_transmission":{"transmissionFactor":1.0}}});
        if volume {
            m["extensions"]["KHR_materials_volume"] = json!({"thicknessFactor":0.4});
        }
        let mut g = document(m);
        fix_twice(&mut g, &settings);
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

    let mut thin = document(
        json!({"pbrMetallicRoughness":{"baseColorFactor":[0.2,0.5,0.8,0.3]},
        "extensions":{"KHR_materials_transmission":{"transmissionFactor":1.0},
            "KHR_materials_volume":{"thicknessFactor":0.0}}}),
    );
    fix_twice(&mut thin, &settings);
    assert_eq!(
        thin["materials"][0]["pbrMetallicRoughness"]["baseColorFactor"],
        json!([1.0, 1.0, 1.0, 0.3])
    );
    assert_eq!(
        thin["materials"][0]["extensions"]["KHR_materials_volume"]["attenuationColor"],
        json!([0.2, 0.5, 0.8])
    );
}

#[test]
fn emissive_product_matches_within_the_documented_tolerance() {
    const {
        assert!(EMISSIVE_PRODUCT_REL_TOLERANCE > 0.0);
        assert!(EMISSIVE_PRODUCT_REL_TOLERANCE <= 1e-9);
    };
    let cases = [
        (json!([4.0, 2.0, -1.0]), Some(3.0_f64)),
        (json!([1.1, 0.3, 0.0]), Some(2.5)),
        (json!([0.2, 1.0, 3.5]), None),
    ];
    for (color, strength) in cases {
        let mut material = json!({"emissiveFactor": color});
        if let Some(strength) = strength {
            material["extensions"]["KHR_materials_emissive_strength"]["emissiveStrength"] = json!(strength);
        }
        let old_strength = strength.unwrap_or(1.0);
        let old_color = material["emissiveFactor"].as_array().expect("rgb").clone();
        let mut g = document(material);
        let report = fix_twice(&mut g, &MaterialLintSettings::default());
        assert!(
            report
                .findings
                .iter()
                .filter(|f| f.code == "emissive_strength" || f.code == "factors_in_range")
                .all(|f| f.effect == MaterialRepairEffect::PreservesAppearance),
            "{:?}",
            report.findings
        );
        let new_color = g["materials"][0]["emissiveFactor"].as_array().expect("rgb");
        let new_strength =
            g["materials"][0]["extensions"]["KHR_materials_emissive_strength"]["emissiveStrength"]
                .as_f64()
                .expect("strength");
        for (old, new) in old_color.iter().zip(new_color.iter()) {
            let old = old.as_f64().expect("channel");
            let new = new.as_f64().expect("channel");
            if old < 0.0 {
                assert_eq!(new, 0.0);
                continue;
            }
            let old_product = old * old_strength;
            let new_product = new * new_strength;
            let scale = old_product.abs().max(1.0);
            let error = (new_product - old_product).abs();
            assert!(
                error <= EMISSIVE_PRODUCT_REL_TOLERANCE * scale,
                "product {old_product} vs {new_product} (error {error})"
            );
        }
    }
}

#[test]
fn findings_stay_within_the_per_material_bound() {
    const {
        assert!(MAX_FINDINGS_PER_MATERIAL <= 32);
    };
    let mut materials = Vec::new();
    for _ in 0..4 {
        materials.push(json!({
            "name":"Coated lamp",
            "pbrMetallicRoughness":{
                "metallicRoughnessTexture":{"index":0},
                "metallicFactor":-0.4,
                "roughnessFactor":4.0,
                "baseColorFactor":[-1.0,2.0,0.5,3.0]
            },
            "emissiveFactor":[4.0,2.0,-1.0],
            "extensions":{
                "KHR_materials_emissive_strength":{"emissiveStrength":3.0},
                "KHR_materials_transmission":{"transmissionFactor":2.0},
                "KHR_materials_volume":{"thicknessFactor":0.4}
            }
        }));
    }
    let mut g = json!({"asset":{"version":"2.0"},"materials":materials});
    let settings = MaterialLintSettings {
        profile: MaterialLintProfile::Compatibility,
        default_volume_thickness: Some(0.2),
        default_attenuation_distance: Some(0.3),
        name_rules: vec![MaterialNameRule {
            contains: "lamp".into(),
            metallic: Some(0.0),
            roughness: None,
            emissive_strength: Some(8.0),
        }],
        ..Default::default()
    };
    let report = fix_twice(&mut g, &settings);
    let cap = 4 * MAX_FINDINGS_PER_MATERIAL + 1;
    assert!(report.findings.len() <= cap, "{} > {cap}", report.findings.len());
    assert!(!report.findings.is_empty());
}

#[test]
fn profile_json_defaults_to_standard_and_rejects_unknown_names() {
    let settings: MaterialLintSettings = serde_json::from_value(json!({})).expect("defaults");
    assert_eq!(settings.profile, MaterialLintProfile::Standard);
    assert!(!settings.profile.changes_appearance());
    let settings: MaterialLintSettings =
        serde_json::from_value(json!({"profile":"compatibility"})).expect("profile");
    assert_eq!(settings.profile, MaterialLintProfile::Compatibility);
    assert!(settings.profile.changes_appearance());
    assert_eq!(settings.profile.name(), "compatibility");
    assert_eq!(MaterialLintProfile::Standard.name(), "standard");
    assert!(serde_json::from_value::<MaterialLintSettings>(json!({"profile":"game"})).is_err());
}
