// SPDX-License-Identifier: MPL-2.0
//! Sub-asset ids across re-imports end to end (ADR-2428, `docs/specs/12-assets.md` § 2): a glTF
//! node and material renamed between cooks keep their sub-asset GUIDs (the `.meta`, the cooked
//! manifest and the model's part list all name the old ids, so references survive); a material
//! removed from the file is reported as `asset_subasset_missing` with the project file that still
//! references it; a source and its `.meta` moved apart are re-paired by content hash on the next
//! cook and keep their id.

use std::path::{Path, PathBuf};

use base64::Engine;
use regia_asset::{AssetId, AssetManifest, MetaSidecar, cooked_cache_dir, cooked_file_name, meta_path_for};
use regia_asset_pipeline::{CookOptions, CookReport, cook_project, host_platform};

/// One node with a two-primitive triangle mesh; primitive 1 uses `metal` (or no material).
fn gltf(node: &str, skin: &str, metal: Option<&str>) -> String {
    let mut bin: Vec<u8> = Vec::new();
    for p in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
        for c in p {
            bin.extend(c.to_le_bytes());
        }
    }
    for i in [0u32, 1, 2] {
        bin.extend(i.to_le_bytes());
    }
    let second_material = if metal.is_some() { r#","material":1"# } else { "" };
    let materials = match metal {
        Some(m) => format!(
            r#"[{{"name":"{skin}"}},{{"name":"{m}","pbrMetallicRoughness":{{"metallicFactor":1.0}}}}]"#
        ),
        None => format!(r#"[{{"name":"{skin}"}}]"#),
    };
    format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],
        "nodes":[{{"name":"{node}","mesh":0}}],
        "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1,"material":0}},
                                  {{"attributes":{{"POSITION":0}},"indices":1{second_material}}}]}}],
        "materials":{materials},
        "buffers":[{{"byteLength":{},"uri":"data:application/octet-stream;base64,{}"}}],
        "bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36}},{{"buffer":0,"byteOffset":36,"byteLength":12}}],
        "accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},
                     {{"bufferView":1,"componentType":5125,"count":3,"type":"SCALAR"}}]}}"#,
        bin.len(),
        base64::engine::general_purpose::STANDARD.encode(&bin),
    )
}

fn cook(root: &Path) -> CookReport {
    let report = cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook");
    // Level files are read as they are, never cooked: the cook lists them as not cookable.
    let errors: Vec<_> = report
        .errors
        .iter()
        .filter(|(_, e)| !e.contains("unsupported source extension"))
        .collect();
    assert!(errors.is_empty(), "cook errors: {errors:?}");
    report
}

fn cache(root: &Path) -> PathBuf {
    cooked_cache_dir(root, host_platform())
}

fn sub_ids(source: &Path) -> Vec<(String, AssetId)> {
    let bytes = std::fs::read(meta_path_for(source)).expect("meta");
    let meta = MetaSidecar::from_ron(&bytes).expect("meta parses");
    meta.sub_assets.into_iter().map(|s| (s.path, s.id)).collect()
}

fn id_of(ids: &[(String, AssetId)], path: &str) -> AssetId {
    ids.iter()
        .find(|(p, _)| p == path)
        .map(|(_, id)| *id)
        .unwrap_or_else(|| panic!("no sub-asset {path} in {ids:?}"))
}

fn manifest_id(root: &Path, path: &str) -> Option<AssetId> {
    let manifest = AssetManifest::read_from_cache(&cache(root))
        .expect("manifest")
        .expect("present");
    manifest
        .entries
        .into_iter()
        .find(|e| e.path == path)
        .map(|e| e.id)
}

fn model_parts(root: &Path, source: &Path) -> Vec<(AssetId, Option<AssetId>)> {
    let model_id = id_of(&sub_ids(source), "model");
    let bytes = std::fs::read(cache(root).join(cooked_file_name(model_id))).expect("model file");
    let (_, payload) = regia_asset::decode_cooked_file(&bytes).expect("cooked");
    let model = regia_asset::CookedModel::from_postcard(payload).expect("model");
    model.parts.iter().map(|p| (p.mesh, p.material)).collect()
}

#[test]
fn renamed_node_and_material_keep_their_ids_and_a_removed_one_warns() {
    let root = regia_test_utils::unique_temp_dir("subasset-rename");
    let assets = root.join("assets");
    std::fs::create_dir_all(assets.join("levels")).expect("dirs");
    let source = assets.join("hero.gltf");
    std::fs::write(&source, gltf("Body", "Skin", Some("Metal"))).expect("gltf");
    cook(&root);
    let first = sub_ids(&source);
    let body0 = id_of(&first, "mesh/Body/primitive_0");
    let body1 = id_of(&first, "mesh/Body/primitive_1");
    let skin = id_of(&first, "material/Skin");
    let metal = id_of(&first, "material/Metal");
    // A level that references the material and the mesh by id.
    std::fs::write(
        assets.join("levels/intro.level.ron"),
        format!("(entities: [(mesh: \"{body0}\", material: \"{skin}\"), (material: \"{metal}\")])"),
    )
    .expect("level");

    // Rename the node and one material in the DCC tool, then re-import.
    std::fs::write(&source, gltf("Hero", "SkinB", Some("Metal"))).expect("renamed");
    let report = cook(&root);
    let second = sub_ids(&source);
    assert_eq!(id_of(&second, "mesh/Hero/primitive_0"), body0);
    assert_eq!(id_of(&second, "mesh/Hero/primitive_1"), body1);
    assert_eq!(id_of(&second, "material/SkinB"), skin);
    assert_eq!(id_of(&second, "material/Metal"), metal);
    assert!(
        second
            .iter()
            .all(|(p, _)| !p.starts_with("mesh/Body") && p != "material/Skin")
    );
    let remapped: Vec<&String> = report
        .warnings
        .iter()
        .filter(|w| w.starts_with("asset_subasset_remapped"))
        .collect();
    assert_eq!(remapped.len(), 3, "{:?}", report.warnings);
    assert!(
        !report
            .warnings
            .iter()
            .any(|w| w.starts_with("asset_subasset_missing")),
        "{:?}",
        report.warnings
    );
    // The manifest and the model name the old ids, so the level's references still resolve.
    assert_eq!(manifest_id(&root, "assets/hero.gltf#material/SkinB"), Some(skin));
    assert_eq!(
        manifest_id(&root, "assets/hero.gltf#mesh/Hero/primitive_0"),
        Some(body0)
    );
    assert_eq!(manifest_id(&root, "assets/hero.gltf#material/Skin"), None);
    assert_eq!(
        model_parts(&root, &source),
        vec![(body0, Some(skin)), (body1, Some(metal))]
    );
    // The first pass's files for the derived ids are gone.
    let parent = MetaSidecar::from_ron(&std::fs::read(meta_path_for(&source)).expect("meta"))
        .expect("meta")
        .id;
    for path in ["material/SkinB", "mesh/Hero/primitive_0"] {
        let stray = cache(&root).join(cooked_file_name(AssetId::sub_asset(parent, path)));
        assert!(!stray.exists(), "stray first-pass file for {path}");
    }
    // A third cook with no change keeps everything (stored ids win).
    std::fs::write(&source, gltf("Hero", "SkinB", Some("Metal"))).expect("touch");
    cook(&root);
    assert_eq!(sub_ids(&source), second);

    // Remove the Metal material: its id is gone, and the warning names the level.
    std::fs::write(&source, gltf("Hero", "SkinB", None)).expect("removed");
    let report = cook(&root);
    let missing: Vec<&String> = report
        .warnings
        .iter()
        .filter(|w| w.starts_with("asset_subasset_missing"))
        .collect();
    assert_eq!(missing.len(), 1, "{:?}", report.warnings);
    assert!(
        missing[0].contains("assets/hero.gltf#material/Metal"),
        "{}",
        missing[0]
    );
    assert!(missing[0].contains(&metal.to_string()), "{}", missing[0]);
    assert!(
        missing[0].contains("assets/levels/intro.level.ron"),
        "{}",
        missing[0]
    );
    assert_eq!(id_of(&sub_ids(&source), "material/SkinB"), skin);
    let log = regia_asset_pipeline::subasset_remap::read_changes_log(&root);
    assert!(
        log.iter()
            .any(|e| e["code"] == "asset_subasset_missing" && e["path"] == "material/Metal")
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_hand_edited_meta_path_re_maps_a_rename_the_rules_cannot_decide() {
    let root = regia_test_utils::unique_temp_dir("subasset-manual");
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("dirs");
    let source = assets.join("prop.gltf");
    std::fs::write(&source, gltf("Prop", "A", Some("B"))).expect("gltf");
    cook(&root);
    let first = sub_ids(&source);
    let a = id_of(&first, "material/A");
    // Both materials renamed at once with equal-looking content: point A's entry at "C" by hand.
    let meta_path = meta_path_for(&source);
    let mut meta = MetaSidecar::from_ron(&std::fs::read(&meta_path).expect("meta")).expect("parse");
    for sub in &mut meta.sub_assets {
        if sub.path == "material/A" {
            sub.path = "material/C".into();
        }
    }
    std::fs::write(&meta_path, meta.to_ron_string().expect("ron")).expect("edited meta");
    std::fs::write(&source, gltf("Prop", "C", Some("B"))).expect("renamed");
    let report = cook(&root);
    assert_eq!(id_of(&sub_ids(&source), "material/C"), a);
    assert!(
        !report.warnings.iter().any(|w| w.starts_with("asset_subasset")),
        "a hand re-map is exact: {:?}",
        report.warnings
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_moved_source_and_meta_re_pair_on_the_next_cook() {
    let root = regia_test_utils::unique_temp_dir("subasset-move");
    let assets = root.join("assets");
    std::fs::create_dir_all(assets.join("props")).expect("dirs");
    std::fs::create_dir_all(assets.join("stray")).expect("dirs");
    let source = assets.join("crate.gltf");
    std::fs::write(&source, gltf("Crate", "Wood", None)).expect("gltf");
    cook(&root);
    let parent = MetaSidecar::from_ron(&std::fs::read(meta_path_for(&source)).expect("meta"))
        .expect("meta")
        .id;
    let wood = id_of(&sub_ids(&source), "material/Wood");
    // Move the source and its meta separately (the meta to the wrong folder).
    let moved = assets.join("props/crate_big.gltf");
    std::fs::rename(&source, &moved).expect("move source");
    std::fs::rename(meta_path_for(&source), assets.join("stray/crate.gltf.meta")).expect("move meta");
    let report = cook(&root);
    assert!(
        report.warnings.iter().any(|w| w
            .starts_with("asset_meta_repaired assets/stray/crate.gltf.meta -> assets/props/crate_big.gltf")),
        "{:?}",
        report.warnings
    );
    let meta =
        MetaSidecar::from_ron(&std::fs::read(meta_path_for(&moved)).expect("re-paired meta")).expect("parse");
    assert_eq!(meta.id, parent, "the GUID survives the move");
    assert!(!assets.join("stray/crate.gltf.meta").exists());
    assert_eq!(manifest_id(&root, "assets/props/crate_big.gltf"), Some(parent));
    assert_eq!(
        manifest_id(&root, "assets/props/crate_big.gltf#material/Wood"),
        Some(wood),
        "sub-asset ids follow the parent"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_live_cooker_re_pairs_a_moved_source_before_cooking_it() {
    let root = regia_test_utils::unique_temp_dir("subasset-live-move");
    let assets = root.join("assets");
    std::fs::create_dir_all(assets.join("moved")).expect("dirs");
    let source = assets.join("barrel.gltf");
    std::fs::write(&source, gltf("Barrel", "Steel", None)).expect("gltf");
    cook(&root);
    let parent = MetaSidecar::from_ron(&std::fs::read(meta_path_for(&source)).expect("meta"))
        .expect("meta")
        .id;
    // The editor's watcher sees only the new path; the sidecar stayed behind.
    let moved = assets.join("moved/barrel.gltf");
    std::fs::rename(&source, &moved).expect("move");
    let targets: std::collections::BTreeSet<PathBuf> = [moved.clone()].into_iter().collect();
    let cooked = regia_asset_pipeline::cook_sources(&CookOptions::for_host(root.clone()), &targets)
        .expect("live cook");
    assert!(cooked.iter().all(|c| c.error.is_none()), "{cooked:?}");
    let meta =
        MetaSidecar::from_ron(&std::fs::read(meta_path_for(&moved)).expect("re-paired")).expect("parse");
    assert_eq!(meta.id, parent, "the live cook kept the GUID");
    assert!(!meta_path_for(&source).exists());
    let _ = std::fs::remove_dir_all(&root);
}
