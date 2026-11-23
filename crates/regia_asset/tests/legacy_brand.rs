// SPDX-License-Identifier: MPL-2.0
//! Cooked files written before the rename to Regia Engine still load (ADR-1388, ADR-1429).
//!
//! A cooked header names its payload type by the FNV-1a hash of the type path. Before the rename
//! the engine's crates had other names, so every `.fckd` and pak entry on disk carries the hash of
//! the old spelling; the loaders must expect exactly those values.

use regia_asset::AssetTypeTag;

#[test]
fn type_tags_are_the_values_written_before_the_rename() {
    // FNV-1a 64 of each type path in the legacy crate spelling: the bytes v0.1.x wrote.
    let expected = [
        ("mesh", AssetTypeTag::mesh(), 0x196b_6b59_6713_3bc7_u64),
        ("gpu_mesh", AssetTypeTag::gpu_mesh(), 0xf2a8_2ee7_ab04_f6ae),
        (
            "mesh_legacy_cpu",
            AssetTypeTag::mesh_legacy_cpu(),
            0xecae_79fc_f587_092a,
        ),
        ("image", AssetTypeTag::image(), 0xaf4c_edbd_a41f_4b13),
        ("texture", AssetTypeTag::texture(), 0x1d5b_9e09_3090_7f81),
        ("heightfield", AssetTypeTag::heightfield(), 0x7639_6739_11f3_bf58),
        ("model", AssetTypeTag::model(), 0x5a35_24ba_4cf5_f06b),
        (
            "standard_material",
            AssetTypeTag::standard_material(),
            0x8bc0_f516_d425_2c36,
        ),
    ];
    for (name, tag, value) in expected {
        assert_eq!(tag.0.get(), value, "{name}: {:016x}", tag.0.get());
    }
}

/// A project's cook cache and pak written by the v0.1.29 CLI (its `new` and `cook` commands, run
/// on two small glTFs and a PNG), kept byte for byte: the manifest names the legacy crates and
/// every header carries the legacy type tag.
fn v0129_cook() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy-brand/v0129-cook")
}

/// Loads one cooked file with the loader its (current-spelling) type path selects; returns the
/// type's short name.
fn load_entry(entry: &regia_asset::ManifestEntry, bytes: &[u8]) -> String {
    let short = entry.type_path.rsplit("::").next().unwrap_or_default().to_owned();
    let what = format!("{} ({})", entry.path, entry.type_path);
    match short.as_str() {
        "CookedMesh" => {
            regia_asset::load_mesh_bytes(bytes).unwrap_or_else(|e| panic!("{what}: {e}"));
        }
        "CookedModel" => {
            regia_asset::load_model_bytes(bytes).unwrap_or_else(|e| panic!("{what}: {e}"));
        }
        "CookedStandardMaterial" => {
            regia_asset::load_material_bytes(bytes).unwrap_or_else(|e| panic!("{what}: {e}"));
        }
        "CookedTexture" => {
            regia_asset::load_texture_bytes(bytes).unwrap_or_else(|e| panic!("{what}: {e}"));
        }
        _ => {
            let (header, _) =
                regia_asset::decode_cooked_file(bytes).unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(
                header.type_tag,
                regia_core::id::TypePathHash::of(&entry.type_path),
                "{what}: tag"
            );
        }
    }
    short
}

#[test]
fn a_cook_cache_written_by_v0129_loads() {
    let dir = v0129_cook();
    let manifest = regia_asset::AssetManifest::read_from_cache(&dir)
        .expect("the v0.1.29 manifest decodes")
        .expect("present");
    assert_eq!(manifest.entries.len(), 10);
    let mut kinds = std::collections::BTreeMap::<String, usize>::new();
    for entry in &manifest.entries {
        assert!(
            entry.type_path.starts_with(regia_core::brand::CRATE_PREFIX),
            "read with the current crate names: {}",
            entry.type_path
        );
        let bytes = std::fs::read(dir.join(regia_asset::cooked_file_name(entry.id))).expect("cooked file");
        *kinds.entry(load_entry(entry, &bytes)).or_default() += 1;
    }
    let kinds: Vec<(&str, usize)> = kinds.iter().map(|(k, n)| (k.as_str(), *n)).collect();
    assert_eq!(
        kinds,
        [
            ("CookedMesh", 4),
            ("CookedModel", 2),
            ("CookedStandardMaterial", 1),
            ("CookedTexture", 1),
            ("PrefabSocketSet", 2),
        ]
    );
}

#[test]
fn a_pak_written_by_v0129_loads() {
    let pak = regia_asset::PakReader::open(&v0129_cook().join("base.fpak")).expect("the v0.1.29 pak opens");
    let entries = pak.manifest().to_vec();
    assert_eq!(entries.len(), 10);
    for entry in &entries {
        assert!(entry.type_path.starts_with(regia_core::brand::CRATE_PREFIX));
        let bytes = pak.read_decompressed(entry.id).expect("entry");
        load_entry(entry, &bytes);
    }
}
