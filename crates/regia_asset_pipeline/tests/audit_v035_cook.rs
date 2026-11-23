// SPDX-License-Identifier: MPL-2.0
//! Regression tests for the audit-v035-cook findings (asset pipeline and cook, v0.2.14
//! candidate). Each test failed before its fix:
//!
//! - M1: a deleted source, or a mesh deleted from a glTF, stayed in the cooked manifest (and so in
//!   the package).
//! - M2: editing only the external `.bin` of a `.glb` left the primary mesh file stale.
//! - M4: sub-asset names that collide after the one retry shared one id (one material or mesh
//!   silently replaced another).
//! - M5: a terrain tile file missing from the cache was never cooked again (the skip trusted the
//!   index file alone).
//! - L1: a short `NORMAL` accessor cut the mesh to its length instead of being an error.
//! - L2: `CookedMesh::validate` accepted submesh ranges past the index buffer.
//! - L3: `CookedImage` and `Heightfield` accepted a zero side.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use image::ImageEncoder;
use regia_asset::{
    AssetId, AssetManifest, CookedImage, Heightfield, cooked_cache_dir, cooked_file_name, meta_path_for,
};
use regia_asset_pipeline::{
    CookOptions, CookReport, Forward, TextureCookQuality, cook_project, host_platform, import_gltf_glb_bytes,
};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------------------------

/// A binary buffer under construction and its buffer views (4-byte aligned).
#[derive(Default)]
struct Bin {
    bytes: Vec<u8>,
    views: Vec<Value>,
}

impl Bin {
    fn view(&mut self, data: &[u8]) -> usize {
        while !self.bytes.len().is_multiple_of(4) {
            self.bytes.push(0);
        }
        let offset = self.bytes.len();
        self.bytes.extend_from_slice(data);
        self.views
            .push(json!({"buffer": 0, "byteOffset": offset, "byteLength": data.len()}));
        self.views.len() - 1
    }
}

fn f32_bytes(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// A GLB container: 12-byte header, JSON chunk, BIN chunk (omitted when `bin` is empty).
fn glb(json: &Value, bin: &[u8]) -> Vec<u8> {
    let mut json_bytes = serde_json::to_vec(json).expect("json");
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let mut bin = bin.to_vec();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let bin_chunk = if bin.is_empty() { 0 } else { 8 + bin.len() };
    let total = 12 + 8 + json_bytes.len() + bin_chunk;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json_bytes);
    if !bin.is_empty() {
        out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        out.extend_from_slice(b"BIN\0");
        out.extend_from_slice(&bin);
    }
    out
}

/// Triangle accessors (positions 0, normals 1, UVs 2, indices 3) appended to `bin`, with
/// `normal_count` normals (3 for a well-formed triangle).
fn triangle(bin: &mut Bin, offset: f32, normal_count: usize) -> Vec<Value> {
    let positions = bin.view(&f32_bytes(&[
        offset,
        0.0,
        0.0,
        offset + 1.0,
        0.0,
        0.0,
        offset,
        1.0,
        0.0,
    ]));
    let normals = bin.view(&f32_bytes(&[0.0, 0.0, 1.0].repeat(normal_count)));
    let uvs = bin.view(&f32_bytes(&[0.0, 0.0, 1.0, 0.0, 0.0, 1.0]));
    let indices = bin.view(
        &[0u16, 1, 2]
            .iter()
            .flat_map(|i| i.to_le_bytes())
            .collect::<Vec<u8>>(),
    );
    vec![
        json!({"bufferView": positions, "componentType": 5126, "count": 3, "type": "VEC3",
               "min": [offset, 0.0, 0.0], "max": [offset + 1.0, 1.0, 0.0]}),
        json!({"bufferView": normals, "componentType": 5126, "count": normal_count, "type": "VEC3"}),
        json!({"bufferView": uvs, "componentType": 5126, "count": 3, "type": "VEC2"}),
        json!({"bufferView": indices, "componentType": 5123, "count": 3, "type": "SCALAR"}),
    ]
}

fn import(bytes: &[u8]) -> Result<regia_asset_pipeline::GltfImportResult, regia_asset_pipeline::ImportError> {
    import_gltf_glb_bytes(
        bytes,
        AssetId::new_random(),
        Forward::Convert,
        "linux",
        TextureCookQuality::High,
    )
}

fn sub_paths(result: &regia_asset_pipeline::GltfImportResult) -> Vec<String> {
    result.sub_assets.iter().map(|s| s.path.clone()).collect()
}

fn assert_unique(paths: &[String]) {
    let unique: BTreeSet<&String> = paths.iter().collect();
    assert_eq!(unique.len(), paths.len(), "sub-asset paths collide: {paths:?}");
}

/// A GLB whose nodes (`name`, mesh index) each draw one of `meshes` triangles.
fn named_meshes_glb(nodes: &[(&str, usize)], meshes: &[&str]) -> Vec<u8> {
    let mut bin = Bin::default();
    let mut accessors = Vec::new();
    let mut mesh_values = Vec::new();
    for (i, name) in meshes.iter().enumerate() {
        let base = accessors.len();
        accessors.extend(triangle(&mut bin, i as f32 * 2.0, 3));
        mesh_values.push(json!({"name": name, "primitives": [{"attributes":
            {"POSITION": base, "NORMAL": base + 1, "TEXCOORD_0": base + 2}, "indices": base + 3}]}));
    }
    let node_values: Vec<Value> = nodes
        .iter()
        .map(|(name, mesh)| json!({"name": name, "mesh": mesh}))
        .collect();
    let json = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": (0..nodes.len()).collect::<Vec<_>>()}],
        "nodes": node_values,
        "meshes": mesh_values,
        "accessors": accessors,
        "bufferViews": bin.views,
        "buffers": [{"byteLength": bin.bytes.len()}],
    });
    glb(&json, &bin.bytes)
}

fn png_rgba(seed: u8) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(16 * 16 * 4);
    for i in 0..16 * 16u32 {
        pixels.extend_from_slice(&[(i as u8) ^ seed, seed, 7, 255]);
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&pixels, 16, 16, image::ExtendedColorType::Rgba8)
        .expect("png encode");
    out
}

fn cook(root: &Path) -> CookReport {
    let report = cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook");
    assert!(report.errors.is_empty(), "cook errors: {:?}", report.errors);
    report
}

fn cache(root: &Path) -> PathBuf {
    cooked_cache_dir(root, host_platform())
}

fn manifest_paths(root: &Path) -> Vec<String> {
    AssetManifest::read_from_cache(&cache(root))
        .expect("manifest")
        .expect("present")
        .entries
        .iter()
        .map(|e| e.path.clone())
        .collect()
}

fn manifest_id(root: &Path, path: &str) -> Option<AssetId> {
    AssetManifest::read_from_cache(&cache(root))
        .expect("manifest")
        .expect("present")
        .entries
        .iter()
        .find(|e| e.path == path)
        .map(|e| e.id)
}

fn project(tag: &str) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    std::fs::write(
        root.join("Regia.toml"),
        "[project]\nname = \"audit-cook\"\nid = \"dev.regia.audit_cook\"\nversion = \"0.1.0\"\n",
    )
    .expect("Regia.toml");
    root
}

// ---------------------------------------------------------------------------------------------
// M4: sub-asset name collisions
// ---------------------------------------------------------------------------------------------

#[test]
fn materials_whose_retry_name_is_taken_still_get_distinct_sub_assets() {
    // Index 2 ("a") retries to `material/a~2`, which material 0 already spells.
    let mut bin = Bin::default();
    let accessors = triangle(&mut bin, 0.0, 3);
    let prim = |material: usize| json!({"attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}, "indices": 3, "material": material});
    let json = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"name": "Tri", "mesh": 0}],
        "meshes": [{"name": "Tri", "primitives": [prim(0), prim(1), prim(2)]}],
        "materials": [
            {"name": "a~2", "pbrMetallicRoughness": {"baseColorFactor": [1.0, 0.0, 0.0, 1.0]}},
            {"name": "a", "pbrMetallicRoughness": {"baseColorFactor": [0.0, 1.0, 0.0, 1.0]}},
            {"name": "a", "pbrMetallicRoughness": {"baseColorFactor": [0.0, 0.0, 1.0, 1.0]}},
        ],
        "accessors": accessors,
        "bufferViews": bin.views,
        "buffers": [{"byteLength": bin.bytes.len()}],
    });
    let result = import(&glb(&json, &bin.bytes)).expect("imports");
    let materials: Vec<String> = sub_paths(&result)
        .into_iter()
        .filter(|p| p.starts_with("material/"))
        .collect();
    assert_eq!(materials.len(), 3, "{materials:?}");
    assert_unique(&materials);
    assert_unique(&sub_paths(&result));
}

#[test]
fn nodes_whose_retry_name_is_taken_still_get_distinct_meshes() {
    // Node 2 ("a") retries to `mesh/a~2/primitive_0`, which node 0 already spells.
    let bytes = named_meshes_glb(&[("a~2", 0), ("a", 1), ("a", 2)], &["m0", "m1", "m2"]);
    let result = import(&bytes).expect("imports");
    let meshes: Vec<String> = sub_paths(&result)
        .into_iter()
        .filter(|p| p.starts_with("mesh/"))
        .collect();
    assert_eq!(meshes.len(), 3, "{meshes:?}");
    assert_unique(&meshes);
}

// ---------------------------------------------------------------------------------------------
// L1: short NORMAL accessor
// ---------------------------------------------------------------------------------------------

#[test]
fn a_short_normal_accessor_is_an_error_naming_it() {
    let mut bin = Bin::default();
    let accessors = triangle(&mut bin, 0.0, 2);
    let json = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"name": "Tri", "mesh": 0}],
        "meshes": [{"name": "Tri", "primitives": [{"attributes":
            {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}, "indices": 3}]}],
        "accessors": accessors,
        "bufferViews": bin.views,
        "buffers": [{"byteLength": bin.bytes.len()}],
    });
    let err = match import(&glb(&json, &bin.bytes)) {
        Ok(result) => panic!(
            "a 2-normal, 3-position primitive imported ({} vertices)",
            result.primary_mesh.vertex_count
        ),
        Err(err) => err.to_string(),
    };
    assert!(
        err.contains("NORMAL"),
        "the error names the short accessor: {err}"
    );
}

// ---------------------------------------------------------------------------------------------
// L2 / L3: cooked validation
// ---------------------------------------------------------------------------------------------

#[test]
fn a_submesh_range_past_the_indices_fails_validation() {
    let bytes = named_meshes_glb(&[("Tri", 0)], &["Tri"]);
    let mut mesh = import(&bytes).expect("imports").primary_mesh;
    mesh.validate().expect("the imported mesh is valid");
    assert!(!mesh.submeshes.is_empty(), "a cooked mesh has a submesh");
    mesh.submeshes[0].index_byte_length = u32::MAX;
    assert!(
        mesh.validate().is_err(),
        "a submesh past the index buffer is invalid"
    );
    mesh.submeshes[0].index_byte_length = 2;
    mesh.submeshes[0].index_byte_offset = 1;
    assert!(
        mesh.validate().is_err(),
        "a submesh starting inside an index is invalid"
    );
}

#[test]
fn zero_sided_images_and_heightfields_fail_validation() {
    let image = CookedImage {
        width: 0,
        height: 8,
        rgba8: Vec::new(),
    };
    assert!(image.validate().is_err());
    assert!(CookedImage::from_postcard(&image.to_postcard().expect("encode")).is_err());
    let field = Heightfield {
        width: 4,
        height: 0,
        samples: Vec::new(),
    };
    assert!(field.validate().is_err());
    let ok = CookedImage {
        width: 1,
        height: 1,
        rgba8: vec![0; 4],
    };
    ok.validate().expect("a 1x1 image is valid");
}

// ---------------------------------------------------------------------------------------------
// M1: stale manifest rows
// ---------------------------------------------------------------------------------------------

#[test]
fn a_deleted_source_leaves_the_manifest_and_the_cache() {
    let root = project("audit-cook-deleted");
    std::fs::write(root.join("assets/keep.png"), png_rgba(1)).expect("keep");
    std::fs::write(root.join("assets/gone.png"), png_rgba(2)).expect("gone");
    cook(&root);
    let gone_id = manifest_id(&root, "assets/gone.png").expect("gone.png cooked");
    assert!(cache(&root).join(cooked_file_name(gone_id)).is_file());
    std::fs::remove_file(root.join("assets/gone.png")).expect("delete");
    let _ = std::fs::remove_file(root.join("assets/gone.png.meta"));
    cook(&root);
    let paths = manifest_paths(&root);
    assert!(paths.iter().any(|p| p == "assets/keep.png"), "{paths:?}");
    assert!(
        !paths.iter().any(|p| p.starts_with("assets/gone.png")),
        "a deleted source is still in the manifest: {paths:?}"
    );
    assert!(
        !cache(&root).join(cooked_file_name(gone_id)).exists(),
        "its cooked file left the cache"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The deletion above must not reach a moved source (ADR-2528): moved with its `.meta` it keeps its
/// id, so the old path's rows name the very file the new path's row owns. Only the old rows go.
#[test]
fn a_source_moved_with_its_meta_keeps_its_cooked_file() {
    let root = project("audit-cook-moved");
    let old = root.join("assets/old.png");
    let new = root.join("assets/new.png");
    std::fs::write(&old, png_rgba(3)).expect("source");
    cook(&root);
    let id = manifest_id(&root, "assets/old.png").expect("old.png cooked");
    assert!(cache(&root).join(cooked_file_name(id)).is_file());

    std::fs::rename(&old, &new).expect("move source");
    std::fs::rename(meta_path_for(&old), meta_path_for(&new)).expect("move meta");
    cook(&root);
    assert_eq!(
        manifest_id(&root, "assets/new.png"),
        Some(id),
        "the id moved with the .meta"
    );
    let paths = manifest_paths(&root);
    assert!(
        !paths.iter().any(|p| p.starts_with("assets/old.png")),
        "the old path is still in the manifest: {paths:?}"
    );
    assert!(
        cache(&root).join(cooked_file_name(id)).is_file(),
        "the moved source's cooked file left the cache"
    );

    // An incremental cook finds it in place, and deleting the moved source still removes it.
    cook(&root);
    assert!(cache(&root).join(cooked_file_name(id)).is_file());
    std::fs::remove_file(&new).expect("delete");
    let _ = std::fs::remove_file(meta_path_for(&new));
    cook(&root);
    assert!(
        !cache(&root).join(cooked_file_name(id)).exists(),
        "a real deletion still clears the cache"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_mesh_deleted_from_a_model_leaves_the_manifest() {
    let root = project("audit-cook-submesh");
    let path = root.join("assets/hero.glb");
    std::fs::write(&path, named_meshes_glb(&[("A", 0), ("B", 1)], &["A", "B"])).expect("hero");
    cook(&root);
    let before = manifest_paths(&root);
    assert!(
        before.iter().any(|p| p.starts_with("assets/hero.glb#mesh/B/")),
        "{before:?}"
    );
    std::fs::write(&path, named_meshes_glb(&[("A", 0)], &["A"])).expect("hero without B");
    cook(&root);
    let after = manifest_paths(&root);
    assert!(
        after.iter().any(|p| p.starts_with("assets/hero.glb#mesh/A/")),
        "{after:?}"
    );
    assert!(
        !after.iter().any(|p| p.starts_with("assets/hero.glb#mesh/B/")),
        "a deleted mesh still resolves: {after:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// M2: `.glb` with an external buffer
// ---------------------------------------------------------------------------------------------

#[test]
fn editing_the_external_buffer_of_a_glb_recooks_its_primary_mesh() {
    let root = project("audit-cook-glb-bin");
    let write = |offset: f32| {
        let mut bin = Bin::default();
        let accessors = triangle(&mut bin, offset, 3);
        let json = json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"name": "Hero", "mesh": 0}],
            "meshes": [{"name": "Hero", "primitives": [{"attributes":
                {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}, "indices": 3}]}],
            "accessors": accessors,
            "bufferViews": bin.views,
            "buffers": [{"byteLength": bin.bytes.len(), "uri": "hero.bin"}],
        });
        std::fs::write(root.join("assets/hero.glb"), glb(&json, &[])).expect("hero.glb");
        std::fs::write(root.join("assets/hero.bin"), &bin.bytes).expect("hero.bin");
    };
    write(0.0);
    cook(&root);
    let id = manifest_id(&root, "assets/hero.glb").expect("hero cooked");
    let first = std::fs::read(cache(&root).join(cooked_file_name(id))).expect("primary");
    // Same `.glb` bytes, new vertex data in the `.bin` only (same length).
    write(5.0);
    cook(&root);
    let second = std::fs::read(cache(&root).join(cooked_file_name(id))).expect("primary");
    assert_ne!(first, second, "the primary mesh still holds the old geometry");
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// M5: terrain skip with a missing tile file
// ---------------------------------------------------------------------------------------------

#[test]
fn a_terrain_file_missing_from_the_cache_is_cooked_again() {
    use regia_terrain::layers::{LayerDesc, TERRAIN_LAYERS_FORMAT_VERSION, TerrainLayers};
    let root = project("audit-cook-terrain");
    let tex = root.join("assets/terrain");
    std::fs::create_dir_all(&tex).expect("terrain dir");
    let layers = TerrainLayers {
        version: TERRAIN_LAYERS_FORMAT_VERSION,
        wetness_default: 0.0,
        layers: vec![LayerDesc {
            name: "ground".to_string(),
            ..LayerDesc::ground()
        }],
        puddles: true,
    };
    std::fs::write(tex.join("terrain.layers.ron"), layers.to_ron().expect("stack")).expect("layers");
    let size = 256u32;
    let samples: Vec<u16> = (0..size * size).map(|i| (i * 37 % 65_521) as u16).collect();
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            ne_bytes(&samples).as_slice(),
            size,
            size,
            image::ExtendedColorType::L16,
        )
        .expect("png");
    std::fs::write(tex.join("heightfield.png"), png).expect("heightfield");
    cook(&root);
    let entry = "assets/terrain/terrain.layers.ron#terrain/heightfield";
    let id = manifest_id(&root, entry).unwrap_or_else(|| panic!("no {entry} in {:?}", manifest_paths(&root)));
    let file = cache(&root).join(cooked_file_name(id));
    assert!(file.is_file());
    std::fs::remove_file(&file).expect("partial cache clean");
    cook(&root);
    assert!(file.is_file(), "the missing terrain file was not cooked again");
    let _ = std::fs::remove_dir_all(&root);
}

/// `u16` samples as native-endian bytes (the layout the `image` encoder takes for `L16`).
fn ne_bytes(samples: &[u16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_ne_bytes()).collect()
}

fn sidecar_meta_project(as_glb: bool) -> PathBuf {
    let root = project("sidecar-meta");
    let mut bin = Bin::default();
    let accessors = triangle(&mut bin, 0.0, 3);
    let json = json!({
        "asset": {"version": "2.0"}, "scene": 0, "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}, "indices": 3, "material": 0}]}],
        "materials": [{"pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}],
        "textures": [{"source": 0}], "images": [{"uri": "paint%20color.png"}],
        "accessors": accessors, "bufferViews": bin.views,
        "buffers": [{"byteLength": bin.bytes.len(), "uri": "mesh.bin"}]
    });
    if as_glb {
        std::fs::write(root.join("assets/model.glb"), glb(&json, &[])).expect("glb");
    } else {
        std::fs::write(
            root.join("assets/model.gltf"),
            serde_json::to_vec(&json).expect("json"),
        )
        .expect("gltf");
    }
    std::fs::write(root.join("assets/mesh.bin"), &bin.bytes).expect("buffer");
    std::fs::write(root.join("assets/paint color.png"), png_rgba(23)).expect("image");
    root
}

#[test]
fn sidecar_image_gets_meta() {
    for as_glb in [false, true] {
        let root = sidecar_meta_project(as_glb);
        let image = root.join("assets/paint color.png");
        cook(&root);
        assert!(
            meta_path_for(&image).is_file(),
            "a referenced image needs a persistent .meta"
        );
        let bytes = std::fs::read(meta_path_for(&image)).expect("image meta");
        let meta = regia_asset::MetaSidecar::from_ron(&bytes).expect("valid meta");
        assert_eq!(meta.importer, "png");
        assert!(
            !root.join("assets/mesh.bin.meta").exists(),
            "buffers are not image assets"
        );
        // One texture for the image, no duplicate: ADR-8055 (glTF external-image dedupe, after
        // this test was written) publishes the image's own row under its `.meta` id, and the glTF
        // keeps no private `#texture/0` copy of it.
        let paths = manifest_paths(&root);
        assert_eq!(
            paths
                .iter()
                .filter(|p| p.starts_with("assets/paint color.png"))
                .count(),
            1,
            "one texture row for the image: {paths:?}"
        );
        assert!(
            !paths
                .iter()
                .any(|p| p.starts_with("assets/model.gl") && p.contains("#texture/")),
            "no private duplicate texture: {paths:?}"
        );
        assert_eq!(
            manifest_id(&root, "assets/paint color.png"),
            Some(meta.id),
            "the shared texture is the image's identity (ADR-8055)"
        );
        cook(&root);
        assert_eq!(
            std::fs::read(meta_path_for(&image)).expect("image meta"),
            bytes,
            "recook preserves identity and settings"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn sidecar_meta_preserves_authored_settings_and_reports_corruption() {
    let root = sidecar_meta_project(false);
    let path = root.join("assets/paint color.png.meta");
    let mut meta = regia_asset::MetaSidecar::new("png", AssetId::new_random());
    meta.settings = Some("(max_size: Some(4), compress: Some(false))".into());
    meta.provenance = Some(regia_asset::Provenance {
        origin: Some("dcc:authored".into()),
        ..Default::default()
    });
    let bytes = meta.to_ron_string().expect("metadata").into_bytes();
    regia_test_utils::write_atomic(&path, &bytes).expect("write");
    cook(&root);
    assert_eq!(std::fs::read(&path).expect("metadata"), bytes);
    // ADR-8055 (after this test was written): the glTF's external image cooks as the image's own
    // shared texture, under the image's `.meta` id, not as a private `#texture/0` sub-asset.
    let texture_id = manifest_id(&root, "assets/paint color.png").expect("shared image texture");
    assert_eq!(
        texture_id, meta.id,
        "the shared texture keeps the authored image identity"
    );
    assert_eq!(manifest_id(&root, "assets/model.gltf#texture/0"), None);
    let cooked = std::fs::read(cache(&root).join(cooked_file_name(texture_id))).expect("texture");
    let (_, payload) = regia_asset::decode_cooked_file(&cooked).expect("cooked header");
    let texture = regia_asset::CookedImage::from_postcard(payload).expect("uncompressed image payload");
    assert_eq!(
        (texture.width, texture.height),
        (4, 4),
        "authored image settings still reach its owner"
    );
    let options = CookOptions::for_host(root.clone());
    let result = regia_asset_pipeline::live::cook_sources(
        &options,
        &std::collections::BTreeSet::from([root.join("assets/model.gltf")]),
    )
    .expect("live cook");
    assert!(result.iter().all(|r| r.error.is_none()), "{result:?}");
    assert_eq!(std::fs::read(&path).expect("metadata"), bytes);
    regia_test_utils::write_atomic(&path, b"invalid metadata").expect("corrupt");
    let report = cook_project(options).expect("report");
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert_eq!(report.errors[0].0, "assets/model.gltf");
    assert!(report.errors[0].1.contains("paint color.png.meta"), "{report:?}");
    assert_eq!(std::fs::read(&path).expect("metadata"), b"invalid metadata");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn live_cook_recreates_a_deleted_sidecar_meta_before_keying() {
    let root = sidecar_meta_project(false);
    cook(&root);
    let path = root.join("assets/paint color.png.meta");
    std::fs::remove_file(&path).expect("remove metadata");
    let targets = std::collections::BTreeSet::from([root.join("assets/model.gltf")]);
    let options = CookOptions::for_host(root.clone());
    let result = regia_asset_pipeline::live::cook_sources(&options, &targets).expect("live cook");
    assert!(result.iter().all(|r| r.error.is_none()), "{result:?}");
    assert!(path.is_file());
    let bytes = std::fs::read(&path).expect("metadata");
    let result = regia_asset_pipeline::live::cook_sources(&options, &targets).expect("live recook");
    assert!(
        result.iter().all(|r| r.error.is_none() && r.skipped),
        "{result:?}"
    );
    assert_eq!(std::fs::read(&path).expect("metadata"), bytes);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn sidecar_metadata_symlink_cannot_leave_the_project() {
    let root = sidecar_meta_project(false);
    let outside = project("sidecar-meta-outside");
    let target = outside.join("untouched.meta");
    regia_test_utils::write_atomic(&target, b"outside metadata").expect("outside");
    std::os::unix::fs::symlink(&target, root.join("assets/paint color.png.meta")).expect("symlink");
    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert!(report.errors[0].1.contains("outside the project"), "{report:?}");
    assert_eq!(std::fs::read(&target).expect("outside"), b"outside metadata");
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(outside);
}

#[test]
fn shared_jpeg_sidecar_keeps_existing_metadata_and_parent_texture_ids() {
    let root = sidecar_meta_project(false);
    let source = root.join("assets/model.gltf");
    let json = std::fs::read_to_string(&source)
        .expect("gltf")
        .replace("paint%20color.png", "paint%20color.jpg");
    regia_test_utils::write_atomic(&source, json.as_bytes()).expect("gltf");
    regia_test_utils::write_atomic(&root.join("assets/second.gltf"), json.as_bytes()).expect("second owner");
    std::fs::remove_file(root.join("assets/paint color.png")).expect("replace source format");
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
        .write_image(
            &[70, 120, 90].repeat(16 * 16),
            16,
            16,
            image::ExtendedColorType::Rgb8,
        )
        .expect("jpeg");
    regia_test_utils::write_atomic(&root.join("assets/paint color.jpg"), &jpeg).expect("image");
    // A source format change may preserve the author's old metadata verbatim.
    let meta = regia_asset::MetaSidecar::new("png", AssetId::new_random());
    let bytes = meta.to_ron_string().expect("metadata").into_bytes();
    let path = root.join("assets/paint color.jpg.meta");
    regia_test_utils::write_atomic(&path, &bytes).expect("metadata");
    cook(&root);
    assert_eq!(std::fs::read(&path).expect("metadata"), bytes);
    // ADR-8055 (after this test was written): both models share the image's one texture, under
    // the id its existing `.meta` already holds (the stale `png` importer name does not mint a new
    // one), and neither keeps a private `#texture/0` copy.
    let shared = manifest_id(&root, "assets/paint color.jpg").expect("shared texture");
    assert_eq!(shared, meta.id);
    assert_eq!(manifest_id(&root, "assets/model.gltf#texture/0"), None);
    assert_eq!(manifest_id(&root, "assets/second.gltf#texture/0"), None);
    let again = cook(&root);
    assert_eq!(again.imports_skipped, 2);
    assert_eq!(manifest_id(&root, "assets/paint color.jpg"), Some(shared));
    assert_eq!(std::fs::read(&path).expect("metadata"), bytes);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn missing_sidecar_image_does_not_create_orphan_metadata() {
    let root = sidecar_meta_project(false);
    std::fs::remove_file(root.join("assets/paint color.png")).expect("remove image");
    let report = cook_project(CookOptions::for_host(root.clone())).expect("report");
    assert!(!report.errors.is_empty());
    assert!(!root.join("assets/paint color.png.meta").exists());
    let _ = std::fs::remove_dir_all(root);
}
