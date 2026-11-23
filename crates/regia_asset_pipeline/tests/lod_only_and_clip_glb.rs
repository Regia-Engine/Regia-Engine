// SPDX-License-Identifier: MPL-2.0
//! LOD-only glTF files and skinned clip GLBs (ADR-7959).
//!
//! A file whose triangle meshes are all `_LODn` with n > 0 used to fail with
//! `no mesh found in glTF file`, because the primary mesh was only an `_LOD0`
//! or an untagged mesh. These fixtures are the shapes that failed: one LOD per
//! file, an LOD1+LOD2 file, and a clip GLB whose bind mesh is `_LOD2`.

use std::path::{Path, PathBuf};

use regia_asset::{
    AssetId, AssetManifest, CookedMesh, cooked_cache_dir, cooked_file_name, decode_cooked_file,
};
use regia_asset_pipeline::gltf_anim::{GltfAnimSettings, import_gltf_animation};
use regia_asset_pipeline::{
    CookOptions, Forward, GltfImportResult, ImportError, TextureCookQuality, cook_project,
    import_gltf_glb_bytes,
};

fn glb(json: &str, bin: &[u8]) -> Vec<u8> {
    let mut json_bytes = json.as_bytes().to_vec();
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let mut bin = bin.to_vec();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let mut out = Vec::new();
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json_bytes);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    out
}

fn num(v: f32) -> String {
    format!("{v:.1}")
}

/// One triangle per named node. `mark` is the first vertex's X, so two meshes
/// never cook to the same payload.
fn mesh_document(nodes: &[(&str, f32)], buffer_uri: Option<&str>) -> (String, Vec<u8>) {
    let mut bin = Vec::new();
    let mut accessors = String::new();
    let mut views = String::new();
    let mut meshes = String::new();
    let mut node_json = String::new();
    for (i, (name, mark)) in nodes.iter().enumerate() {
        let pos_off = bin.len();
        let corners = [[*mark, 0.0, 0.0], [*mark, 1.0, 0.0], [*mark + 0.5, 0.0, 1.0]];
        for corner in corners {
            for component in corner {
                bin.extend_from_slice(&component.to_le_bytes());
            }
        }
        let idx_off = bin.len();
        for index in [0u32, 1, 2] {
            bin.extend_from_slice(&index.to_le_bytes());
        }
        if i > 0 {
            accessors.push(',');
            views.push(',');
            meshes.push(',');
            node_json.push(',');
        }
        let mark_s = num(*mark);
        let max_x = num(*mark + 0.5);
        accessors.push_str(&format!(
            r#"{{"bufferView":{},"componentType":5126,"count":3,"type":"VEC3","min":[{mark_s},0.0,0.0],"max":[{max_x},1.0,1.0]}},{{"bufferView":{},"componentType":5125,"count":3,"type":"SCALAR"}}"#,
            i * 2,
            i * 2 + 1,
        ));
        views.push_str(&format!(
            r#"{{"buffer":0,"byteOffset":{pos_off},"byteLength":36,"target":34962}},{{"buffer":0,"byteOffset":{idx_off},"byteLength":12,"target":34963}}"#
        ));
        meshes.push_str(&format!(
            r#"{{"primitives":[{{"attributes":{{"POSITION":{}}},"indices":{},"mode":4}}]}}"#,
            i * 2,
            i * 2 + 1,
        ));
        node_json.push_str(&format!(r#"{{"mesh":{i},"name":"{name}"}}"#));
    }
    let scene_nodes = (0..nodes.len())
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let buffer = match buffer_uri {
        Some(uri) => format!(r#"{{"uri":"{uri}","byteLength":{}}}"#, bin.len()),
        None => format!(r#"{{"byteLength":{}}}"#, bin.len()),
    };
    let json = format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[{scene_nodes}]}}],"nodes":[{node_json}],"meshes":[{meshes}],"accessors":[{accessors}],"bufferViews":[{views}],"buffers":[{buffer}]}}"#
    );
    (json, bin)
}

fn meshes_glb(nodes: &[(&str, f32)]) -> Vec<u8> {
    let (json, bin) = mesh_document(nodes, None);
    glb(&json, &bin)
}

fn import_glb(bytes: &[u8]) -> Result<GltfImportResult, ImportError> {
    import_gltf_glb_bytes(
        bytes,
        AssetId::new_random(),
        Forward::Keep,
        "linux",
        TextureCookQuality::Low,
    )
}

fn mesh_paths(imported: &GltfImportResult) -> Vec<&str> {
    imported
        .sub_assets
        .iter()
        .filter(|sub| sub.type_path == "regia_asset::CookedMesh")
        .map(|sub| sub.path.as_str())
        .collect()
}

fn primary_path(imported: &GltfImportResult) -> String {
    let bytes = imported.primary_mesh.to_postcard().expect("primary postcard");
    imported
        .sub_assets
        .iter()
        .find(|sub| sub.payload == bytes)
        .map(|sub| sub.path.clone())
        .expect("primary mesh is one of the mesh sub-assets")
}

fn lod_levels(imported: &GltfImportResult) -> Vec<u32> {
    let mut levels = Vec::new();
    for group in &imported.lod_groups {
        for level in &group.levels {
            levels.push(level.level);
        }
    }
    levels
}

#[test]
fn lod2_only_file_imports_with_the_first_lod2_as_primary() {
    let imported = import_glb(&meshes_glb(&[
        ("SM_Arsenal_LOD2", 2.0),
        ("SM_Arsenal_Iris_LOD2", 3.0),
    ]))
    .expect("LOD2-only file imports");
    assert_eq!(
        primary_path(&imported),
        "mesh/SM_Arsenal_LOD2/primitive_0",
        "the only LOD index is the primary, and the first mesh of that index wins"
    );
    assert_eq!(
        mesh_paths(&imported),
        [
            "mesh/SM_Arsenal_LOD2/primitive_0",
            "mesh/SM_Arsenal_Iris_LOD2/primitive_0",
        ]
    );
    assert!(
        imported.lod_groups.is_empty(),
        "a single LOD index is not a group"
    );
}

#[test]
fn lod1_and_lod2_file_uses_the_lowest_index_and_keeps_visit_order() {
    // LOD2 is visited first. The primary is still LOD1. Sub-assets stay in
    // visit order. No LodGroup: a group still requires LOD0, as before.
    let imported = import_glb(&meshes_glb(&[("SM_Bar_LOD2", 2.0), ("SM_Bar_LOD1", 1.0)]))
        .expect("LOD1+LOD2 file imports");
    assert_eq!(primary_path(&imported), "mesh/SM_Bar_LOD1/primitive_0");
    assert_eq!(
        mesh_paths(&imported),
        ["mesh/SM_Bar_LOD2/primitive_0", "mesh/SM_Bar_LOD1/primitive_0"]
    );
    assert!(imported.lod_groups.is_empty());
}

#[test]
fn a_higher_lod_index_does_not_beat_a_lower_one() {
    let imported = import_glb(&meshes_glb(&[("Wide_LOD10", 10.0), ("Wide_LOD2", 2.0)]))
        .expect("LOD2+LOD10 file imports");
    assert_eq!(primary_path(&imported), "mesh/Wide_LOD2/primitive_0");
}

#[test]
fn lod0_file_keeps_lod0_as_primary_and_orders_the_group_by_index() {
    // Visit order is LOD2, LOD0, LOD1. The base rule still picks LOD0, and the
    // group is still sorted 0, 1, 2 rather than visit order.
    let imported = import_glb(&meshes_glb(&[
        ("Keep_LOD2", 2.0),
        ("Keep_LOD0", 0.0),
        ("Keep_LOD1", 1.0),
    ]))
    .expect("LOD0 file imports");
    assert_eq!(primary_path(&imported), "mesh/Keep_LOD0/primitive_0");
    assert_eq!(imported.lod_groups.len(), 1);
    assert_eq!(imported.lod_groups[0].name, "Keep");
    assert_eq!(lod_levels(&imported), vec![0, 1, 2]);
    assert_eq!(
        mesh_paths(&imported),
        [
            "mesh/Keep_LOD2/primitive_0",
            "mesh/Keep_LOD0/primitive_0",
            "mesh/Keep_LOD1/primitive_0",
        ]
    );
}

#[test]
fn a_lone_lod0_is_still_not_a_group() {
    let imported = import_glb(&meshes_glb(&[("Only_LOD0", 0.0)])).expect("lone LOD0 imports");
    assert_eq!(primary_path(&imported), "mesh/Only_LOD0/primitive_0");
    assert!(imported.lod_groups.is_empty());
}

#[test]
fn an_untagged_mesh_still_beats_a_later_lod_tag() {
    let imported = import_glb(&meshes_glb(&[("Body_LOD2", 2.0), ("Body", 7.0)])).expect("import");
    assert_eq!(primary_path(&imported), "mesh/Body/primitive_0");
    assert!(imported.lod_groups.is_empty(), "one tagged level is not a chain");
}

#[test]
fn a_file_with_no_mesh_and_no_animation_still_reports_no_mesh() {
    let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],"nodes":[{"name":"Empty"}]}"#;
    let err = import_glb(&glb(json, &[])).expect_err("empty file");
    assert!(matches!(err, ImportError::NoMesh), "{err}");
    assert_eq!(err.to_string(), "no mesh found in glTF file");
}

/// One skin, two clips, and a single `_LOD2` bind mesh.
fn clip_glb() -> Vec<u8> {
    let mut bin = Vec::new();
    let push = |bin: &mut Vec<u8>, values: &[f32]| {
        for value in values {
            bin.extend_from_slice(&value.to_le_bytes());
        }
    };
    // Inverse bind matrices for the sample joint translations.
    push(
        &mut bin,
        &[
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, -1.0, 0.0, 0.0, 1.0,
        ],
    );
    push(
        &mut bin,
        &[
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, -1.0, -1.0, 0.0, 1.0,
        ],
    );
    push(&mut bin, &[0.0, 1.0]);
    push(&mut bin, &[0.0, 1.0, 0.0, 0.0, 2.0, 0.0]);
    push(&mut bin, &[1.0, 0.0, 0.0, 3.0, 0.0, 0.0]);
    push(&mut bin, &[2.0, 0.0, 0.0, 2.0, 1.0, 0.0, 2.5, 0.0, 1.0]);
    for index in [0u32, 1, 2] {
        bin.extend_from_slice(&index.to_le_bytes());
    }
    for _ in 0..3 {
        bin.extend_from_slice(&0u16.to_le_bytes());
        bin.extend_from_slice(&0u16.to_le_bytes());
        bin.extend_from_slice(&0u16.to_le_bytes());
        bin.extend_from_slice(&0u16.to_le_bytes());
    }
    for _ in 0..3 {
        push(&mut bin, &[1.0, 0.0, 0.0, 0.0]);
    }
    assert_eq!(bin.len(), 128 + 8 + 24 + 24 + 36 + 12 + 24 + 48);

    let json = r#"{
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0, 2]}],
        "nodes": [
            {"name": "root", "translation": [1.0, 0.0, 0.0], "children": [1]},
            {"name": "bone", "translation": [0.0, 1.0, 0.0]},
            {"name": "SK_Pilot_LOD2", "mesh": 0, "skin": 0}
        ],
        "skins": [{"joints": [0, 1], "inverseBindMatrices": 0, "skeleton": 0}],
        "animations": [
            {
                "name": "idle",
                "channels": [{"sampler": 0, "target": {"node": 1, "path": "translation"}}],
                "samplers": [{"input": 1, "output": 2, "interpolation": "LINEAR"}]
            },
            {
                "name": "walk",
                "channels": [{"sampler": 0, "target": {"node": 0, "path": "translation"}}],
                "samplers": [{"input": 1, "output": 3, "interpolation": "LINEAR"}]
            }
        ],
        "meshes": [{
            "primitives": [{
                "attributes": {"POSITION": 4, "JOINTS_0": 6, "WEIGHTS_0": 7},
                "indices": 5,
                "mode": 4
            }]
        }],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": 2, "type": "MAT4"},
            {"bufferView": 1, "componentType": 5126, "count": 2, "type": "SCALAR", "min": [0.0], "max": [1.0]},
            {"bufferView": 2, "componentType": 5126, "count": 2, "type": "VEC3"},
            {"bufferView": 3, "componentType": 5126, "count": 2, "type": "VEC3"},
            {"bufferView": 4, "componentType": 5126, "count": 3, "type": "VEC3", "min": [2.0, 0.0, 0.0], "max": [2.5, 1.0, 1.0]},
            {"bufferView": 5, "componentType": 5125, "count": 3, "type": "SCALAR"},
            {"bufferView": 6, "componentType": 5123, "count": 3, "type": "VEC4"},
            {"bufferView": 7, "componentType": 5126, "count": 3, "type": "VEC4"}
        ],
        "bufferViews": [
            {"buffer": 0, "byteOffset": 0, "byteLength": 128},
            {"buffer": 0, "byteOffset": 128, "byteLength": 8},
            {"buffer": 0, "byteOffset": 136, "byteLength": 24},
            {"buffer": 0, "byteOffset": 160, "byteLength": 24},
            {"buffer": 0, "byteOffset": 184, "byteLength": 36},
            {"buffer": 0, "byteOffset": 220, "byteLength": 12},
            {"buffer": 0, "byteOffset": 232, "byteLength": 24},
            {"buffer": 0, "byteOffset": 256, "byteLength": 48}
        ],
        "buffers": [{"byteLength": 304}]
    }"#;
    glb(json, &bin)
}

#[test]
fn a_skinned_lod2_bind_mesh_imports_with_its_clips() {
    let bytes = clip_glb();
    let imported = import_glb(&bytes).expect("clip GLB imports");
    assert_eq!(primary_path(&imported), "mesh/SK_Pilot_LOD2/primitive_0");
    assert!(
        imported.primary_mesh.skin.is_some(),
        "the LOD2 mesh is the clip set's bind mesh"
    );
    assert!(imported.lod_groups.is_empty());

    let anim = import_gltf_animation(&bytes, &GltfAnimSettings::default()).expect("clips");
    assert_eq!(anim.skins.len(), 1);
    let mut names: Vec<&str> = anim.skins[0]
        .clips
        .iter()
        .map(|clip| clip.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["idle", "walk"]);
}

struct TempProject(PathBuf);

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_bytes(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    let tmp = path.with_extension("partial");
    std::fs::write(&tmp, bytes).expect("write tmp");
    std::fs::rename(&tmp, path).expect("rename");
}

fn payload_of(cache: &Path, manifest: &AssetManifest, path: &str) -> Vec<u8> {
    let entry = manifest
        .entries
        .iter()
        .find(|entry| entry.path == path)
        .unwrap_or_else(|| panic!("manifest missing {path}"));
    let bytes = std::fs::read(cache.join(cooked_file_name(entry.id))).expect("cooked file");
    let (_, payload) = decode_cooked_file(&bytes).expect("cooked header");
    payload.to_vec()
}

#[test]
fn regia_cook_imports_lod_only_files_and_a_clip_glb() {
    let root = regia_test_utils::unique_temp_dir("lod-only-cook");
    let _guard = TempProject(root.clone());
    let buildings = root.join("assets/buildings");
    let anims = root.join("assets/anims");
    let empty = root.join("assets/empty");

    let (arsenal_json, arsenal_bin) = mesh_document(
        &[("SM_Arsenal_LOD2", 2.0), ("SM_Arsenal_Iris_LOD2", 3.0)],
        Some("SM_Arsenal_LOD2.bin"),
    );
    write_bytes(&buildings.join("SM_Arsenal_LOD2.gltf"), arsenal_json.as_bytes());
    write_bytes(&buildings.join("SM_Arsenal_LOD2.bin"), &arsenal_bin);

    let (bar_json, bar_bin) =
        mesh_document(&[("SM_Bar_LOD2", 2.0), ("SM_Bar_LOD1", 1.0)], Some("SM_Bar.bin"));
    write_bytes(&buildings.join("SM_Bar.gltf"), bar_json.as_bytes());
    write_bytes(&buildings.join("SM_Bar.bin"), &bar_bin);

    let (keep_json, keep_bin) = mesh_document(
        &[("Keep_LOD2", 2.0), ("Keep_LOD0", 0.0), ("Keep_LOD1", 1.0)],
        Some("Keep.bin"),
    );
    write_bytes(&buildings.join("SM_Keep.gltf"), keep_json.as_bytes());
    write_bytes(&buildings.join("Keep.bin"), &keep_bin);

    write_bytes(&anims.join("pilot.glb"), &clip_glb());
    write_bytes(
        &empty.join("blank.gltf"),
        br#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],"nodes":[{"name":"Empty"}]}"#,
    );

    // The same importer `regia cook` runs (`cook_project`). One bad file is recorded
    // and the others still cook. The CLI still refuses `pack_paks` on that error.
    let report = cook_project(CookOptions {
        project_root: root.clone(),
        platform: "linux".to_string(),
        incremental: false,
    })
    .expect("cook returns a report");
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert_eq!(report.errors[0].0, "assets/empty/blank.gltf");
    assert_eq!(report.errors[0].1, "import error: no mesh found in glTF file");
    assert_eq!(report.cooked, 4, "{report:?}");

    let cache = cooked_cache_dir(&root, "linux");
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest read")
        .expect("manifest present");

    let arsenal_primary = payload_of(&cache, &manifest, "assets/buildings/SM_Arsenal_LOD2.gltf");
    let arsenal_mesh = payload_of(
        &cache,
        &manifest,
        "assets/buildings/SM_Arsenal_LOD2.gltf#mesh/SM_Arsenal_LOD2/primitive_0",
    );
    assert_eq!(arsenal_primary, arsenal_mesh);
    assert!(
        manifest
            .entries
            .iter()
            .any(|entry| entry.path.contains("SM_Arsenal_Iris_LOD2"))
    );
    assert!(!manifest.entries.iter().any(|entry| {
        entry
            .path
            .starts_with("assets/buildings/SM_Arsenal_LOD2.gltf#lod_group/")
    }));

    let bar_primary = payload_of(&cache, &manifest, "assets/buildings/SM_Bar.gltf");
    let bar_lod1 = payload_of(
        &cache,
        &manifest,
        "assets/buildings/SM_Bar.gltf#mesh/SM_Bar_LOD1/primitive_0",
    );
    assert_eq!(bar_primary, bar_lod1, "lowest LOD index is the cooked primary");
    assert!(
        manifest
            .entries
            .iter()
            .any(|entry| { entry.path == "assets/buildings/SM_Bar.gltf#mesh/SM_Bar_LOD2/primitive_0" })
    );

    let keep_primary = payload_of(&cache, &manifest, "assets/buildings/SM_Keep.gltf");
    let keep_lod0 = payload_of(
        &cache,
        &manifest,
        "assets/buildings/SM_Keep.gltf#mesh/Keep_LOD0/primitive_0",
    );
    assert_eq!(
        keep_primary, keep_lod0,
        "an LOD0 file still cooks LOD0 as primary"
    );
    assert!(manifest.entries.iter().any(|entry| {
        entry.path.starts_with("assets/buildings/SM_Keep.gltf#lod_group/")
            && entry.type_path == "regia_asset::LodGroup"
    }));

    let pilot = payload_of(&cache, &manifest, "assets/anims/pilot.glb");
    let bind = payload_of(
        &cache,
        &manifest,
        "assets/anims/pilot.glb#mesh/SK_Pilot_LOD2/primitive_0",
    );
    assert_eq!(pilot, bind, "the LOD2 mesh is the cooked bind mesh");
    let mesh = CookedMesh::from_postcard(&pilot).expect("bind mesh");
    assert!(mesh.skin.is_some());
    for (path, type_path) in [
        (
            "assets/anims/pilot.glb#clip/idle",
            "regia_animation::AnimationClip",
        ),
        (
            "assets/anims/pilot.glb#clip/walk",
            "regia_animation::AnimationClip",
        ),
        (
            "assets/anims/pilot.glb#skeleton/root",
            "regia_animation::Skeleton",
        ),
    ] {
        let entry = manifest
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("missing {path}"));
        assert_eq!(entry.type_path, type_path);
    }
    assert!(
        !manifest
            .entries
            .iter()
            .any(|entry| entry.path.starts_with("assets/empty/"))
    );
}
