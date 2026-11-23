// SPDX-License-Identifier: MPL-2.0
//! The all-LOD loader's shared images respect the resident texture cap and do not copy
//! texture data. A test binary of its own: the cap is process-wide state.

use regia_asset::{AssetId, AssetServer, AssetServerSettings, AssetTypeTag, CookedHeader};
use regia_asset_pipeline::{Forward, TextureCookQuality, import_gltf_path};
use regia_render::cooked_model::{CookedModelLods, gpu_mesh_file_from_cooked, load_cooked_models_all_lods};
use regia_render::{LabMaterial, MaterialImage, SharedImage, TextureStreaming};
use regia_test_utils::TempDir;
use serde_json::json;

/// The cap this binary runs under: far below the 64-texel fixture images.
const CAP: u32 = 16;

fn png(width: u32, height: u32, pixel: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("PNG header");
    writer
        .write_image_data(&pixel.repeat((width * height) as usize))
        .expect("PNG pixels");
    writer.finish().expect("PNG end");
    out
}

/// A glTF with two materials. `Block` has a 64 x 64 base color (block-compressed by the
/// cook) and a 64 x 64 normal map; `Strip` has a 64 x 2 base color (too thin to
/// block-compress: an RGBA8 image) and the same normal map. Returns the root, the glTF id
/// and the strip image's cooked id.
fn project() -> (TempDir, AssetId, AssetId) {
    let root = TempDir::new("all-lods-cap");
    let id = AssetId::new_random();
    let mut bytes = Vec::new();
    for v in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
        for c in v {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
    }
    for uv in [[0.0f32, 0.0], [1.0, 0.0], [0.0, 1.0]] {
        for c in uv {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
    }
    for i in [0u32, 1, 2] {
        bytes.extend_from_slice(&i.to_le_bytes());
    }
    std::fs::write(root.join("mesh.bin"), &bytes).expect("buffer");
    std::fs::write(root.join("block.png"), png(64, 64, [200, 120, 40, 255])).expect("block");
    std::fs::write(root.join("strip.png"), png(64, 2, [40, 160, 220, 255])).expect("strip");
    std::fs::write(root.join("normal.png"), png(64, 64, [128, 128, 255, 255])).expect("normal");
    let source = json!({
        "asset": {"version": "2.0"},
        "buffers": [{"uri": "mesh.bin", "byteLength": bytes.len()}],
        "bufferViews": [
            {"buffer": 0, "byteLength": 36},
            {"buffer": 0, "byteOffset": 36, "byteLength": 24},
            {"buffer": 0, "byteOffset": 60, "byteLength": 12}
        ],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
             "min": [0,0,0], "max": [1,1,0]},
            {"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC2"},
            {"bufferView": 2, "componentType": 5125, "count": 3, "type": "SCALAR"}
        ],
        "images": [{"uri": "block.png"}, {"uri": "strip.png"}, {"uri": "normal.png"}],
        "textures": [{"source": 0}, {"source": 1}, {"source": 2}],
        "materials": [
            {"name": "Block", "pbrMetallicRoughness": {"baseColorTexture": {"index": 0}},
             "normalTexture": {"index": 2}},
            {"name": "Strip", "pbrMetallicRoughness": {"baseColorTexture": {"index": 1}},
             "normalTexture": {"index": 2}}
        ],
        "meshes": [
            {"primitives": [{"attributes": {"POSITION": 0, "TEXCOORD_0": 1}, "indices": 2, "material": 0}]},
            {"primitives": [{"attributes": {"POSITION": 0, "TEXCOORD_0": 1}, "indices": 2, "material": 1}]}
        ],
        "nodes": [{"name": "Block", "mesh": 0}, {"name": "Strip", "mesh": 1}],
        "scenes": [{"nodes": [0, 1]}], "scene": 0
    });
    let path = root.join("panel.gltf");
    std::fs::write(&path, serde_json::to_vec(&source).expect("json")).expect("gltf");
    let cooked = import_gltf_path(&path, id, Forward::Keep, "linux", None, TextureCookQuality::High)
        .expect("cook glTF");
    let cache = regia_asset::cooked_cache_dir(&root, &regia_asset::current_platform());
    std::fs::create_dir_all(&cache).expect("cache");
    let mut strip = None;
    for sub in cooked.sub_assets {
        let tag = match sub.type_path.as_str() {
            "regia_asset::CookedMesh" => AssetTypeTag::mesh(),
            "regia_asset::CookedModel" => AssetTypeTag::model(),
            "regia_asset::CookedStandardMaterial" => AssetTypeTag::standard_material(),
            "regia_asset::CookedImage" => {
                strip = Some(sub.id);
                AssetTypeTag::image()
            }
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
        // GPU-ready meshes: this test is about images, so it must not depend on the mesh form.
        let file = gpu_mesh_file_from_cooked(&file)
            .expect("offline conversion")
            .unwrap_or(file);
        let dest = cache.join(regia_asset::cooked_file_name(sub.id));
        let tmp = dest.with_extension("tmp");
        std::fs::write(&tmp, file).expect("write");
        std::fs::rename(tmp, dest).expect("publish");
    }
    (root, id, strip.expect("the 64 x 2 image cooks as RGBA8"))
}

fn server(root: &std::path::Path) -> AssetServer {
    AssetServer::open(AssetServerSettings {
        project_root: root.to_path_buf(),
        platform: regia_asset::current_platform(),
        content_dir: None,
    })
    .expect("server")
}

fn shared(image: Option<&MaterialImage>) -> &SharedImage {
    match image {
        Some(MaterialImage::Shared(image)) => image,
        other => panic!("expected a shared image, got {other:?}"),
    }
}

/// The material of the part cooked from node `node`.
fn material<'a>(model: &'a CookedModelLods, node: &str) -> &'a LabMaterial {
    let part = model
        .parts
        .iter()
        .find(|p| p.node == node)
        .unwrap_or_else(|| panic!("part {node}"));
    &model.materials[part.material]
}

#[test]
fn shared_images_fit_the_resident_cap_and_copy_no_texels() {
    regia_render::set_resident_cap(TextureStreaming {
        enabled: true,
        max_px: CAP,
        ..TextureStreaming::DESKTOP_HIGH
    });
    let (root, id, strip_image) = project();
    let models = load_cooked_models_all_lods(&mut server(&root), &[id]).expect("load");
    assert_eq!(models[0].materials.len(), 2);

    // Block: a block-compressed chain. The shared image is the material's fitted chain,
    // level for level the same bytes (no copy), and uploads no larger than the cap.
    let block = material(&models[0], "Block");
    let fitted = block
        .compressed
        .as_ref()
        .and_then(|maps| maps.albedo.as_ref())
        .expect("a cooked chain");
    assert!(
        fitted.width.max(fitted.height) <= CAP,
        "{}x{}",
        fitted.width,
        fitted.height
    );
    let image = shared(block.images.albedo.as_ref());
    assert!(
        image.size().0.max(image.size().1) <= CAP,
        "shared size {:?}",
        image.size()
    );
    assert!(!image.has_pixels(), "no RGBA8 copy of a cooked chain");
    let chain = image.compressed().expect("the shared chain");
    assert_eq!((chain.width, chain.height), (fitted.width, fitted.height));
    assert_eq!(chain.levels.len(), fitted.levels.len());
    for (shared, own) in chain.levels.iter().zip(&fitted.levels) {
        assert_eq!(shared.as_ptr(), own.as_ptr(), "a view of the same bytes");
        assert_eq!(shared.len(), own.len());
    }

    // Strip: an RGBA8 image. The shared pixels are the material's fitted pixels.
    let strip = material(&models[0], "Strip");
    let fitted = strip.albedo.as_ref().expect("the fitted RGBA8 map");
    assert!(
        fitted.width.max(fitted.height) <= CAP,
        "{}x{}",
        fitted.width,
        fitted.height
    );
    let image = shared(strip.images.albedo.as_ref());
    assert_eq!(image.size(), (fitted.width, fitted.height));
    assert_eq!(image.pixels(), fitted.rgba.as_slice());

    // The normal map both materials use: one shared handle, fitted.
    let block_normal = shared(block.images.normal.as_ref());
    let strip_normal = shared(strip.images.normal.as_ref());
    assert_eq!(
        block_normal.identity(),
        strip_normal.identity(),
        "one handle per image"
    );
    assert!(block_normal.size().0.max(block_normal.size().1) <= CAP);

    // Without its base color the strip draws the neutral fallback, which uses no image:
    // its normal map must not be attached through a shared handle either.
    let cache = regia_asset::cooked_cache_dir(&root, &regia_asset::current_platform());
    std::fs::remove_file(cache.join(regia_asset::cooked_file_name(strip_image))).expect("remove strip");
    let models = load_cooked_models_all_lods(&mut server(&root), &[id]).expect("fallback load");
    let fallback = models[0]
        .materials
        .iter()
        .find(|m| m.base_color == LabMaterial::neutral_fallback().base_color)
        .expect("the neutral fallback");
    assert!(fallback.normal_map.is_none() && fallback.compressed.is_none());
    for image in [
        &fallback.images.albedo,
        &fallback.images.normal,
        &fallback.images.occlusion,
        &fallback.images.emissive,
    ] {
        assert!(image.is_none(), "the fallback draws no map: {image:?}");
    }
}
