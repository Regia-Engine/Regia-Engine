// SPDX-License-Identifier: MPL-2.0
//! Rough transmission from a glTF file, end to end (ADR-2732): the importer's opt-in
//! (`extras.regia_transmission_roughness`) reaches the cooked material and, from there, the
//! renderer's material. Without it a transmissive material stays clear glass whatever its core
//! roughness (ADR-2075 (2)). The fixture is `tests/fixtures/gltf/frosted_pane.gltf`.

use regia_asset::{AssetId, CookedStandardMaterial};
use regia_asset_pipeline::{Forward, GltfImportResult, TextureCookQuality, import_gltf_path};
use regia_render::LabMaterial;
use regia_render::hard_surface::{AuthoredSurfaceMaps, ResolvedBaseMaps};
use regia_render::rough_transmission::effective_roughness;

const FIXTURE: &str = include_str!("fixtures/gltf/frosted_pane.gltf");

fn import(json: &str) -> GltfImportResult {
    let dir = regia_test_utils::unique_temp_dir("transmission_roughness_import");
    let path = dir.join("pane.gltf");
    std::fs::write(&path, json).expect("write the glTF");
    let result = import_gltf_path(
        &path,
        AssetId::new_random(),
        Forward::Keep,
        "desktop",
        None,
        TextureCookQuality::High,
    )
    .expect("import");
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn cooked(result: &GltfImportResult, name: &str) -> CookedStandardMaterial {
    let asset = result
        .sub_assets
        .iter()
        .filter(|s| s.type_path == "regia_asset::CookedStandardMaterial")
        .find(|s| s.path == format!("material/{name}"))
        .unwrap_or_else(|| panic!("material {name}"));
    CookedStandardMaterial::from_postcard(&asset.payload).expect("cooked material")
}

/// The material the runtime draws from the cooked one.
fn runtime(material: &CookedStandardMaterial) -> LabMaterial {
    LabMaterial::from_authored_resolved(
        "fixture",
        &material.authored,
        ResolvedBaseMaps::default(),
        AuthoredSurfaceMaps::default(),
    )
}

#[test]
fn the_opt_in_takes_the_core_roughness_through_the_cook_to_the_renderer() {
    let material = cooked(&import(FIXTURE), "M_Core");
    assert_eq!(material.authored.transmission_roughness, Some(0.35));
    let lab = runtime(&material);
    assert_eq!(lab.transmission, 0.9);
    assert_eq!(effective_roughness(&lab), 0.35);
}

#[test]
fn a_number_is_used_as_it_stands() {
    let material = cooked(&import(FIXTURE), "M_Explicit");
    assert_eq!(material.authored.transmission_roughness, Some(0.6));
    assert_eq!(effective_roughness(&runtime(&material)), 0.6);
}

/// ADR-2075 (2): a shipped glass asset must not start blurring on a recook.
#[test]
fn without_the_opt_in_a_rough_transmissive_material_stays_clear() {
    let material = cooked(&import(FIXTURE), "M_NoOptIn");
    assert_eq!(material.authored.transmission_roughness, None);
    assert_eq!(material.authored.base.roughness, 0.4);
    assert_eq!(effective_roughness(&runtime(&material)), 0.0);
}

#[test]
fn a_smooth_or_opaque_material_has_no_transmission_roughness() {
    let result = import(FIXTURE);
    for name in ["M_SmoothCore", "M_Opaque"] {
        assert_eq!(
            cooked(&result, name).authored.transmission_roughness,
            None,
            "{name}"
        );
    }
}

/// A value out of range clamps, and a value of another type is no opt-in.
#[test]
fn odd_values_clamp_or_read_as_no_opt_in() {
    let swap = |value: &str| {
        FIXTURE.replacen(
            "\"regia_transmission_roughness\": 0.6",
            &format!("\"regia_transmission_roughness\": {value}"),
            1,
        )
    };
    let read = |value: &str| {
        cooked(&import(&swap(value)), "M_Explicit")
            .authored
            .transmission_roughness
    };
    assert_eq!(read("4.0"), Some(1.0));
    assert_eq!(read("-1.0"), None);
    assert_eq!(read("false"), None);
    assert_eq!(read("\"rough\""), None);
}

/// The cooked payload keeps the field across its own round trip.
#[test]
fn the_cooked_material_round_trips() {
    let material = cooked(&import(FIXTURE), "M_Core");
    let bytes = material.to_postcard().expect("encode");
    let back = CookedStandardMaterial::from_postcard(&bytes).expect("decode");
    assert_eq!(back.authored.transmission_roughness, Some(0.35));
}
