// SPDX-License-Identifier: MPL-2.0
//! Authored and generated LODs through the real glTF cook and runtime loader.

use std::collections::BTreeMap;

use regia_asset::{AssetId, AssetServer, AssetServerSettings, AssetSource, AssetTypeTag, CookedHeader};
use regia_asset_pipeline::{
    CookOptions, Forward, TextureCookQuality, cook_project, host_platform, import_gltf_path,
    pack_cooked_directory_with,
};
use regia_render::cooked_model::{
    gpu_mesh_file_from_cooked, load_cooked_models, load_cooked_models_all_lods,
};
use regia_test_utils::TempDir;
use serde_json::json;

fn project() -> (TempDir, AssetId) {
    project_with_maps(false, false)
}

fn project_with_maps(textured: bool, generate_each_part: bool) -> (TempDir, AssetId) {
    let root = TempDir::new("all-lods-gltf");
    let id = AssetId::new_random();
    let mut bytes = Vec::new();
    let side = 9u32;
    for y in 0..side {
        for x in 0..side {
            for v in [x as f32, y as f32, 0.0] {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    let vertex_bytes = bytes.len();
    for y in 0..side - 1 {
        for x in 0..side - 1 {
            let a = y * side + x;
            for i in [a, a + 1, a + side, a + 1, a + side + 1, a + side] {
                bytes.extend_from_slice(&i.to_le_bytes());
            }
        }
    }
    let index_bytes = bytes.len() - vertex_bytes;
    std::fs::write(root.join("mesh.bin"), &bytes).expect("buffer");
    let mut source = json!({
        "asset": {"version": "2.0"},
        "buffers": [{"uri": "mesh.bin", "byteLength": bytes.len()}],
        "bufferViews": [
            {"buffer": 0, "byteLength": vertex_bytes},
            {"buffer": 0, "byteOffset": vertex_bytes, "byteLength": index_bytes}
        ],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": side * side,
             "type": "VEC3", "min": [0,0,0], "max": [8,8,0]},
            {"bufferView": 1, "componentType": 5125, "count": index_bytes / 4, "type": "SCALAR"}
        ],
        "materials": [
            {"name": "Near", "pbrMetallicRoughness": {"baseColorFactor": [0.8,0.2,0.1,1],
             "metallicFactor": 0.7, "roughnessFactor": 0.25}, "emissiveFactor": [0.1,0.2,0.3]},
            {"name": "Far", "pbrMetallicRoughness": {"baseColorFactor": [0.1,0.3,0.8,0.5],
             "metallicFactor": 0.2, "roughnessFactor": 0.75}, "alphaMode": "MASK",
             "alphaCutoff": 0.3, "doubleSided": true, "emissiveFactor": [0.3,0.2,0.1]}
        ],
        "meshes": [
            {"primitives": [{"attributes": {"POSITION": 0}, "indices": 1, "material": 0}]},
            {"primitives": [{"attributes": {"POSITION": 0}, "indices": 1, "material": 1}]}
        ],
        "nodes": [{"name": "Hull_LOD0", "mesh": 0}, {"name": "Hull_LOD1", "mesh": 1}],
        "scenes": [{"nodes": [0,1]}], "scene": 0
    });
    if textured {
        source["images"] = json!([{ "uri": "near.png" }, { "uri": "far.png" }]);
        source["textures"] = json!([{ "source": 0 }, { "source": 1 }]);
        for (index, (name, pixel)) in [
            ("near.png", [128, 128, 255, 255]),
            ("far.png", [160, 128, 240, 255]),
        ]
        .into_iter()
        .enumerate()
        {
            let file = std::fs::File::create(root.join(name)).expect("image file");
            let mut encoder = png::Encoder::new(file, 2, 2);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("PNG header");
            writer.write_image_data(&pixel.repeat(4)).expect("PNG pixels");
            writer.finish().expect("PNG end");
            let material = &mut source["materials"][index];
            material["pbrMetallicRoughness"]["baseColorTexture"] = json!({ "index": index });
            material["pbrMetallicRoughness"]["metallicRoughnessTexture"] = json!({ "index": index });
            material["normalTexture"] = json!({ "index": index });
            material["occlusionTexture"] = json!({ "index": index });
            material["emissiveTexture"] = json!({ "index": index });
        }
    }
    if generate_each_part {
        // The importer generates chains only on LOD-zero/non-LOD nodes. Use two
        // non-LOD nodes for the generated-chain fixture; the authored-LOD fixture
        // above stays Hull_LOD0/Hull_LOD1, including its pinned legacy bytes.
        source["nodes"][0]["name"] = json!("HullNear");
        source["nodes"][1]["name"] = json!("HullFar");
    }
    let path = root.join("hull.gltf");
    std::fs::write(&path, serde_json::to_vec(&source).expect("json")).expect("gltf");
    let cooked = import_gltf_path(
        &path,
        id,
        Forward::Keep,
        "linux",
        Some("(generate_lods: Some((count: 3, ratios: [1.0, 0.5, 0.25])))"),
        TextureCookQuality::High,
    )
    .expect("cook glTF");
    let cache = regia_asset::cooked_cache_dir(&root, &regia_asset::current_platform());
    std::fs::create_dir_all(&cache).expect("cache");
    for sub in cooked.sub_assets {
        let tag = match sub.type_path.as_str() {
            "regia_asset::CookedMesh" => AssetTypeTag::mesh(),
            "regia_asset::CookedModel" => AssetTypeTag::model(),
            "regia_asset::CookedStandardMaterial" => AssetTypeTag::standard_material(),
            "regia_asset::CookedImage" => AssetTypeTag::image(),
            "regia_asset::CookedTexture" => AssetTypeTag::texture(),
            _ => continue,
        };
        let header = CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: tag.0,
            asset_id: sub.id,
            importer_version: 1,
            dependencies: sub.dependencies,
            payload_len: sub.payload.len() as u64,
        };
        let file = regia_asset::encode_cooked_file(&header, &sub.payload).expect("wrap");
        let file = gpu_mesh_file_from_cooked(&file)
            .expect("offline conversion")
            .unwrap_or(file);
        let dest = cache.join(regia_asset::cooked_file_name(sub.id));
        let tmp = dest.with_extension("tmp");
        std::fs::write(&tmp, file).expect("write");
        std::fs::rename(tmp, dest).expect("publish");
    }
    // Prove runtime loading needs only the cache.
    std::fs::remove_file(path).expect("remove glTF");
    std::fs::remove_file(root.join("mesh.bin")).expect("remove buffer");
    if textured {
        std::fs::remove_file(root.join("near.png")).expect("remove near image");
        std::fs::remove_file(root.join("far.png")).expect("remove far image");
    }
    (root, id)
}

fn server(root: &std::path::Path) -> AssetServer {
    AssetServer::open(AssetServerSettings {
        project_root: root.to_path_buf(),
        platform: regia_asset::current_platform(),
        content_dir: None,
    })
    .expect("server")
}

#[test]
fn authored_lods_retain_distinct_complete_materials() {
    let (root, id) = project();
    let models = load_cooked_models_all_lods(&mut server(&root), &[id]).expect("load");
    let model = &models[0];
    assert_eq!(model.parts.len(), 2, "both authored LODs must be retained");
    assert_eq!(model.parts[0].node, "Hull_LOD0");
    assert_eq!(model.parts[1].node, "Hull_LOD1");
    assert_eq!(model.parts[0].authored_lod, 0);
    assert_eq!(model.parts[1].authored_lod, 1);
    assert_ne!(model.parts[0].mesh_id, model.parts[1].mesh_id);
    assert_eq!(model.materials.len(), 2);
    assert_ne!(model.material_ids[0], model.material_ids[1]);
    let near = &model.materials[model.parts[0].material];
    let far = &model.materials[model.parts[1].material];
    assert_eq!(near.base_color, [0.8, 0.2, 0.1, 1.0]);
    assert_eq!(far.base_color, [0.1, 0.3, 0.8, 0.5]);
    assert_eq!((near.metallic, near.roughness), (0.7, 0.25));
    assert_eq!((far.metallic, far.roughness), (0.2, 0.75));
    assert_eq!(near.emissive, [0.1, 0.2, 0.3]);
    assert_eq!(far.emissive, [0.3, 0.2, 0.1]);
    assert_eq!(far.alpha_mode, regia_render::ALPHA_MASK);
    assert_eq!(far.alpha_cutoff, 0.3);
    assert!(far.double_sided);
    assert!(!near.double_sided);
}

#[test]
fn generated_index_lods_are_selectable_without_changing_vertices() {
    let (root, id) = project_with_maps(false, true);
    let models = load_cooked_models_all_lods(&mut server(&root), &[id]).expect("load");
    for part in &models[0].parts {
        assert!(part.index_lods.len() >= 2, "real meshopt-generated LODs");
        for pair in part.index_lods.windows(2) {
            assert!(pair[1].index_count < pair[0].index_count);
        }
        for (level, range) in part.index_lods.iter().enumerate() {
            let mesh = part.lab_mesh(level).expect("select level");
            assert_eq!(mesh.indices, part.lod_indices(level).expect("range"));
            assert_eq!(mesh.indices.len(), range.index_count as usize);
            assert_eq!(mesh.material, part.material);
            assert_eq!(
                bytemuck::cast_slice::<_, u8>(&mesh.vertices),
                bytemuck::cast_slice::<_, u8>(&part.vertices)
            );
        }
        assert!(part.lod_indices(part.index_lods.len()).is_none());
        assert!(part.lab_mesh(usize::MAX).is_none());
    }
}

#[test]
fn each_authored_lod_keeps_its_base_normal_orm_and_emissive_maps() {
    let (root, id) = project_with_maps(true, false);
    let models = load_cooked_models_all_lods(&mut server(&root), &[id]).expect("load textures");
    let model = &models[0];
    for (index, pixel) in [[128, 128, 255, 255], [160, 128, 240, 255]].iter().enumerate() {
        let material = &model.materials[model.parts[index].material];
        assert_eq!(material.albedo.as_ref().expect("albedo").rgba, pixel.repeat(4));
        assert_eq!(
            material.normal_map.as_ref().expect("normal").rgba,
            pixel.repeat(4)
        );
        assert_eq!(
            material.emissive_map.as_ref().expect("emissive").rgba,
            pixel.repeat(4)
        );
        assert!(material.occlusion_map.is_some());
        assert!(material.orm_packed);
        assert_eq!(material.normal_scale, 1.0);
        assert!(material.images.albedo.is_some());
        assert!(material.images.normal.is_some());
        assert!(material.images.occlusion.is_some());
        assert!(material.images.emissive.is_some());
    }
}

/// The legacy loader's output for the authored-LOD fixture, field by field. Positions,
/// coverage and materials are exact; generated normals and tangents are checked against
/// their contract with a tolerance, so other platforms' float math and new fields do not
/// break it (this replaced a hash of the `Debug` text).
#[test]
fn legacy_fixture_fields() {
    let (root, id) = project();
    let models = load_cooked_models(&mut server(&root), &[id]).expect("load");
    let draws = &models[0];
    assert_eq!(draws.meshes.len(), 1, "LOD 0 only: Hull_LOD1 is not drawn");
    assert_eq!(draws.materials.len(), 1);
    assert_eq!(draws.texture_bytes, 0);
    let mesh = &draws.meshes[0];
    assert_eq!(mesh.material, 0);
    assert_eq!(mesh.model, regia_render::LabMesh::unskinned().model);
    assert!(mesh.joints.is_empty());
    assert!(mesh.indices.len().is_multiple_of(3));
    assert!(mesh.indices.iter().all(|&i| (i as usize) < mesh.vertices.len()));
    // The cook may reorder vertices; the set of positions is the authored 9 x 9 grid.
    let mut positions: Vec<[i64; 3]> = mesh
        .vertices
        .iter()
        .map(|v| v.position.map(|c| (c * 1.0e4).round() as i64))
        .collect();
    positions.sort_unstable();
    positions.dedup();
    let grid: Vec<[i64; 3]> = (0..9)
        .flat_map(|x| (0..9).map(move |y| [x * 10_000, y * 10_000, 0]))
        .collect();
    assert_eq!(positions, grid);
    // LOD 0 covers the whole 8 x 8 grid once, with no degenerate triangle.
    let mut area = 0.0f64;
    for tri in mesh.indices.as_chunks::<3>().0 {
        let [a, b, c] = tri.map(|i| mesh.vertices[i as usize].position);
        let doubled = f64::from((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]));
        assert!(doubled.abs() > 1.0e-6, "degenerate triangle {tri:?}");
        area += doubled.abs() * 0.5;
    }
    assert!((area - 64.0).abs() < 1.0e-3, "LOD 0 area {area}");
    let z_sign = mesh.vertices[0].normal[2].signum();
    for v in &mesh.vertices {
        let [nx, ny, nz] = v.normal;
        assert!(
            (nz * z_sign - 1.0).abs() < 1.0e-3 && nx.abs() < 1.0e-3 && ny.abs() < 1.0e-3,
            "normal {v:?}"
        );
        let [tx, ty, tz, tw] = v.tangent;
        assert!(
            ((tx * tx + ty * ty + tz * tz).sqrt() - 1.0).abs() < 1.0e-3,
            "unit tangent {v:?}"
        );
        assert!(
            (tx * nx + ty * ny + tz * nz).abs() < 1.0e-3,
            "tangent orthogonal to the normal {v:?}"
        );
        assert!(tw == 1.0 || tw == -1.0, "bitangent sign {v:?}");
        assert_eq!(v.uv, [0.0, 0.0], "the fixture has no UVs");
        assert_eq!(v.color, [1.0; 4], "lab vertices stay white");
        assert_eq!((v.joints, v.weights), ([0; 4], [0.0; 4]), "rigid");
    }
    let near = &draws.materials[0];
    let close = |a: &[f32], b: &[f32]| a.iter().zip(b).all(|(a, b)| (a - b).abs() < 1.0e-6);
    assert!(
        close(&near.base_color, &[0.8, 0.2, 0.1, 1.0]),
        "{:?}",
        near.base_color
    );
    assert!(close(&[near.metallic, near.roughness], &[0.7, 0.25]));
    assert!(close(&near.emissive, &[0.1, 0.2, 0.3]), "{:?}", near.emissive);
    assert_eq!(near.alpha_mode, regia_render::ALPHA_OPAQUE);
    assert!(!near.double_sided && !near.unlit && !near.orm_packed);
    assert_eq!(near.normal_scale, 0.0, "no normal map");
    assert!(near.albedo.is_none() && near.normal_map.is_none());
    assert!(near.occlusion_map.is_none() && near.emissive_map.is_none());
    assert!(near.compressed.is_none());
    let images = &near.images;
    assert!(
        [
            &images.albedo,
            &images.normal,
            &images.occlusion,
            &images.emissive,
            &images.detail,
            &images.detail_normal,
            &images.aux,
        ]
        .iter()
        .all(|image| image.is_none()),
        "the legacy loader sets no shared image"
    );
}

/// A GLB holding one 9 x 9 grid mesh per node (one material). `colored` adds a `COLOR_0`
/// stream (a GPU-ready pak stores it GPU-ready since ADR-7919, the color in the vertex color
/// slot); `morph` adds one morph target, which keeps the mesh compact even in a pak built with
/// GPU-ready meshes (ADR-7919 (4)).
fn grid_glb(nodes: &[&str], colored: bool, morph: bool) -> Vec<u8> {
    let side = 9u32;
    let mut bin = Vec::new();
    for y in 0..side {
        for x in 0..side {
            for v in [x as f32, y as f32, 0.0] {
                bin.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    let vertex_bytes = bin.len();
    for y in 0..side - 1 {
        for x in 0..side - 1 {
            let a = y * side + x;
            for i in [a, a + 1, a + side, a + 1, a + side + 1, a + side] {
                bin.extend_from_slice(&i.to_le_bytes());
            }
        }
    }
    let index_bytes = bin.len() - vertex_bytes;
    let mut views = vec![
        json!({"buffer": 0, "byteLength": vertex_bytes}),
        json!({"buffer": 0, "byteOffset": vertex_bytes, "byteLength": index_bytes}),
    ];
    let mut accessors = vec![
        json!({"bufferView": 0, "componentType": 5126, "count": side * side,
               "type": "VEC3", "min": [0,0,0], "max": [8,8,0]}),
        json!({"bufferView": 1, "componentType": 5125, "count": index_bytes / 4, "type": "SCALAR"}),
    ];
    let mut attributes = json!({"POSITION": 0});
    if colored {
        let start = bin.len();
        for i in 0..side * side {
            for c in [0.9f32, (i % 7) as f32 / 7.0, 0.2, 1.0] {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        views.push(json!({"buffer": 0, "byteOffset": start, "byteLength": bin.len() - start}));
        accessors.push(json!({"bufferView": 2, "componentType": 5126, "count": side * side, "type": "VEC4"}));
        attributes["COLOR_0"] = json!(2);
    }
    let mut primitive = json!({"attributes": attributes, "indices": 1, "material": 0});
    if morph {
        let start = bin.len();
        for i in 0..side * side {
            for d in [0.0f32, 0.0, 0.25 * (i % 3) as f32] {
                bin.extend_from_slice(&d.to_le_bytes());
            }
        }
        views.push(json!({"buffer": 0, "byteOffset": start, "byteLength": bin.len() - start}));
        accessors.push(
            json!({"bufferView": views.len() - 1, "componentType": 5126, "count": side * side,
               "type": "VEC3", "min": [0,0,0], "max": [0,0,0.5]}),
        );
        primitive["targets"] = json!([{"POSITION": accessors.len() - 1}]);
    }
    let source = json!({
        "asset": {"version": "2.0"},
        "buffers": [{"byteLength": bin.len()}],
        "bufferViews": views,
        "accessors": accessors,
        "materials": [{"name": "Paint", "pbrMetallicRoughness": {"baseColorFactor": [0.4,0.5,0.6,1]}}],
        "meshes": nodes.iter().map(|_| json!({"primitives": [primitive]})).collect::<Vec<_>>(),
        "nodes": nodes.iter().enumerate().map(|(i, name)| json!({"name": name, "mesh": i})).collect::<Vec<_>>(),
        "scenes": [{"nodes": (0..nodes.len()).collect::<Vec<_>>()}], "scene": 0
    });
    let mut json_bytes = serde_json::to_vec(&source).expect("json");
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let mut glb = Vec::new();
    for word in [0x4654_6C67u32, 2, total as u32, json_bytes.len() as u32] {
        glb.extend_from_slice(&word.to_le_bytes());
    }
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json_bytes);
    glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"BIN\0");
    glb.extend_from_slice(&bin);
    glb
}

/// The real cook, then the real pak writer with and without GPU-ready meshes, then the
/// all-LOD loader from the pak. In the GPU-ready pak every mesh is GPU-ready: the colored mesh
/// (ADR-7919) and, since GPU mesh layout v3, the morphing mesh with its targets in the trailing
/// block (ADR-10080; with layout v2 it stayed compact). Both pak forms give the same vertices, tangents and index
/// LODs as each other and as the legacy loader's LOD 0: `COLOR_0` stays out of the cooked draw
/// (white vertex color) in both loaders and both forms.
#[test]
fn cooked_paks_load_all_lods_with_and_without_gpu_meshes() {
    let root = TempDir::new("all-lods-pak");
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    for (name, glb) in [
        ("hull.glb", grid_glb(&["Hull_LOD0", "Hull_LOD1"], false, false)),
        ("tinted.glb", grid_glb(&["Tinted"], true, false)),
        ("morph.glb", grid_glb(&["Morph"], false, true)),
    ] {
        std::fs::write(root.join("assets").join(name), glb).expect("glb");
    }
    let report = cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let cache = regia_asset::cooked_cache_dir(&root, host_platform());
    let mut loaded = Vec::new();
    for gpu_meshes in [false, true] {
        let content = root.join(if gpu_meshes {
            "content-gpu"
        } else {
            "content-compact"
        });
        pack_cooked_directory_with(&cache, &content, &BTreeMap::new(), true, gpu_meshes).expect("pak");
        let open = || {
            AssetServer::open(AssetServerSettings {
                project_root: root.to_path_buf(),
                platform: host_platform().to_string(),
                content_dir: Some(content.clone()),
            })
            .expect("server")
        };
        let mut server = open();
        assert!(server.has_paks());
        let ids = ["assets/hull.glb", "assets/tinted.glb", "assets/morph.glb"]
            .map(|path| server.id_for_path(path).expect("id"));
        let models = load_cooked_models_all_lods(&mut server, &ids).expect("all LODs from the pak");
        let forms = server.load_forms();
        let expected = if gpu_meshes { (4, 0) } else { (0, 4) };
        assert_eq!(
            (forms.gpu_meshes, forms.compact_meshes),
            expected,
            "gpu_meshes={gpu_meshes}"
        );
        assert_eq!(models[0].parts.len(), 2);
        assert_eq!(
            models[0].parts.iter().map(|p| p.authored_lod).collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(models[1].parts.len(), 1);
        assert_eq!(models[2].parts.len(), 1);
        for part in models.iter().flat_map(|m| &m.parts) {
            assert_eq!(server.source_of(part.mesh_id), AssetSource::Pak("base".into()));
            assert!(part.lod_indices(0).is_some_and(|i| !i.is_empty()));
        }
        let legacy = load_cooked_models(&mut open(), &ids).expect("legacy from the pak");
        for (model, draws) in models.iter().zip(&legacy) {
            let lod0 = &model.parts[0];
            assert_eq!(
                bytemuck::cast_slice::<_, u8>(&lod0.vertices),
                bytemuck::cast_slice::<_, u8>(&draws.meshes[0].vertices),
                "gpu_meshes={gpu_meshes}: the legacy decode's vertices and tangents"
            );
            assert_eq!(lod0.lod_indices(0), Some(draws.meshes[0].indices.as_slice()));
            assert!(
                lod0.vertices.iter().all(|v| v.color == [1.0; 4]),
                "gpu_meshes={gpu_meshes}: COLOR_0 stays out of the cooked draw"
            );
        }
        loaded.push(models);
    }
    for (compact, gpu) in loaded[0].iter().zip(&loaded[1]) {
        for (a, b) in compact.parts.iter().zip(&gpu.parts) {
            assert_eq!(a.mesh_id, b.mesh_id);
            assert_eq!(
                bytemuck::cast_slice::<_, u8>(&a.vertices),
                bytemuck::cast_slice::<_, u8>(&b.vertices)
            );
            assert_eq!(a.indices, b.indices);
            assert_eq!(a.index_lods, b.index_lods);
        }
    }
}
