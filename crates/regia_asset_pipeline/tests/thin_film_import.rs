// SPDX-License-Identifier: MPL-2.0
//! `KHR_materials_iridescence` import (ADR-1977): the extension reaches the cooked material's thin
//! film, with the extension's defaults, its maps as cooked slots and dependencies, and its
//! channels preserved by the texture cook. A file without the extension, or with a factor of 0,
//! cooks exactly as before. The fixture is `tests/fixtures/gltf/iridescent_feather.gltf`: one
//! triangle, four materials (a full film, a bare block, no extension, a factor of 0).

use regia_asset::{AssetId, CookedStandardMaterial, TextureCookUsage, TextureSlot, ThinFilmParams};
use regia_asset_pipeline::gltf_extensions::{GltfExtensionRegistry, GltfExtensionTable};
use regia_asset_pipeline::{Forward, GltfImportResult, TextureCookQuality, import_gltf_path};

const FIXTURE: &str = include_str!("fixtures/gltf/iridescent_feather.gltf");

/// Imports `json` as a `.gltf` file, as a project's cook does.
fn import(json: &str) -> GltfImportResult {
    let dir = regia_test_utils::unique_temp_dir("thin_film_import");
    let path = dir.join("feather.gltf");
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

fn material(result: &GltfImportResult, name: &str) -> (CookedStandardMaterial, Vec<AssetId>) {
    let asset = result
        .sub_assets
        .iter()
        .filter(|s| s.type_path == "regia_asset::CookedStandardMaterial")
        .find(|s| s.path == format!("material/{name}"))
        .unwrap_or_else(|| panic!("material {name}"));
    (
        CookedStandardMaterial::from_postcard(&asset.payload).expect("cooked material"),
        asset.dependencies.clone(),
    )
}

#[test]
fn the_film_reaches_the_cooked_material_with_its_maps_as_slots_and_dependencies() {
    let result = import(FIXTURE);
    let (cooked, deps) = material(&result, "M_Feather");
    let film = cooked.authored.thin_film.as_ref().expect("thin film");
    assert_eq!(film.factor, 0.9);
    assert_eq!(film.ior, 1.56);
    assert_eq!((film.thickness_min, film.thickness_max), (180.0, 520.0));
    for (name, slot) in [("coverage", &film.factor_map), ("thickness", &film.thickness_map)] {
        let Some(TextureSlot::Asset(id)) = slot else {
            panic!("{name} map is not a cooked slot: {slot:?}");
        };
        assert!(
            deps.contains(id),
            "the {name} map is a dependency of the material"
        );
    }
    assert_ne!(film.factor_map, film.thickness_map);
}

#[test]
fn a_bare_block_takes_the_extension_defaults() {
    let result = import(FIXTURE);
    let (cooked, _) = material(&result, "M_Defaults");
    assert_eq!(
        cooked.authored.thin_film,
        Some(ThinFilmParams {
            factor: 1.0,
            ..ThinFilmParams::default()
        })
    );
    let film = ThinFilmParams::default();
    assert_eq!(film.ior, 1.3);
    assert_eq!((film.thickness_min, film.thickness_max), (100.0, 400.0));
}

#[test]
fn no_extension_and_a_zero_factor_cook_exactly_alike() {
    let result = import(FIXTURE);
    let (bare, _) = material(&result, "M_Bare");
    let (off, _) = material(&result, "M_FilmOff");
    assert_eq!(bare.authored.thin_film, None);
    assert_eq!(off.authored.thin_film, None);
    // The same material bytes but for the name.
    let mut renamed = off.authored.clone();
    renamed.name = bare.authored.name.clone();
    assert_eq!(renamed, bare.authored);
}

#[test]
fn removing_the_extension_leaves_every_other_material_unchanged() {
    let without = FIXTURE.replacen(
        r#"
    "KHR_materials_iridescence": {
     "iridescenceFactor": 1.0
    }
"#,
        "\n",
        1,
    );
    assert_ne!(without, FIXTURE, "the fixture's block changed shape");
    let (a, b) = (import(FIXTURE), import(&without));
    assert_eq!(material(&a, "M_Bare").0, material(&b, "M_Bare").0);
    assert_eq!(
        material(&a, "M_Feather").0.authored.base,
        material(&b, "M_Feather").0.authored.base
    );
    assert_eq!(material(&b, "M_Defaults").0.authored.thin_film, None);
}

#[test]
fn the_side_table_keeps_the_values_and_names_the_textures() {
    let table = GltfExtensionTable::from_bytes(FIXTURE.as_bytes(), &mut GltfExtensionRegistry::new());
    let m = table.material(0).expect("material 0");
    let film = m.iridescence.as_ref().expect("iridescence");
    assert_eq!(film.factor, 0.9);
    assert_eq!(m.textures["iridescence"].image, 0);
    assert_eq!(m.textures["iridescence_thickness"].image, 1);
    assert!(
        table.unknown.is_empty(),
        "the extension is understood: {:?}",
        table.unknown
    );
}

#[test]
fn out_of_range_and_non_numeric_values_read_as_the_extension_allows() {
    let table = |block: &str| {
        let json = format!(r#"{{"materials":[{{"extensions":{{"KHR_materials_iridescence":{block}}}}}]}}"#);
        GltfExtensionTable::from_bytes(json.as_bytes(), &mut GltfExtensionRegistry::new())
    };
    let t = table(
        r#"{"iridescenceFactor":3.0,"iridescenceIor":0.5,"iridescenceThicknessMinimum":-5,
            "iridescenceThicknessMaximum":"thick"}"#,
    );
    let film = t.material(0).and_then(|m| m.iridescence.clone()).expect("block");
    assert_eq!(film.factor, 1.0);
    assert_eq!(film.ior, 1.0);
    assert_eq!(film.thickness_min, 0.0);
    assert_eq!(
        film.thickness_max, 400.0,
        "a value of the wrong type reads as the default"
    );
    // A texture index past the document's list is ignored, not a panic.
    let t = table(r#"{"iridescenceFactor":1.0,"iridescenceTexture":{"index":99}}"#);
    assert!(t.material(0).expect("material").textures.is_empty());
    // Not an object: no block.
    assert!(table("7").material(0).is_none());
}

/// The thickness is in the green channel: its cook keeps every channel.
#[test]
fn the_film_textures_cook_linear_with_all_their_channels() {
    let table = GltfExtensionTable::from_bytes(FIXTURE.as_bytes(), &mut GltfExtensionRegistry::new());
    let usages = table.extension_texture_usages();
    for image in [0usize, 1] {
        assert!(
            usages.contains(&(image, TextureCookUsage::ColorLinear)),
            "image {image}: {usages:?}"
        );
    }
}
