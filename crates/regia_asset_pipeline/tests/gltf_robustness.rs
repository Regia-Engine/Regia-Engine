// SPDX-License-Identifier: MPL-2.0
//! glTF importer robustness (lane sprint-asset-long, ADR-875): unusual but valid files import,
//! hostile ones are errors, never a panic or a stack overflow.
//!
//! - A `.gltf` whose images are grey, grey + alpha or 16-bit PNGs imports, and each image widens to
//!   RGBA8 the way the `image` crate does for the same file inside a `.glb`.
//! - Node cycles and node chains deeper than the importer walks are errors; a deep chain inside the
//!   limit still composes every node's transform.
//! - A structured fuzz mutates the JSON of a skinned, morphing, textured, animated GLB (numbers,
//!   removed keys and array items, node children rewired into cycles) and feeds every mutation to
//!   the mesh and the animation importers; byte-level mutation runs on the same fixture.

use image::ImageEncoder;
use regia_asset::{AssetId, CookedImage, MetaSidecar, meta_path_for};
use regia_asset_pipeline::gltf_anim::{GltfAnimSettings, import_gltf_animation};
use regia_asset_pipeline::{
    Forward, ImportError, TextureCookQuality, import_gltf, import_gltf_glb_bytes, import_gltf_path,
};
use regia_test_utils::fuzz::{FuzzRng, assert_no_panic, fuzz_loader};
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------------------------

/// A binary buffer under construction and its buffer views (4-byte aligned, glTF 2.0 § 3.6.2.4).
#[derive(Default)]
struct Bin {
    bytes: Vec<u8>,
    views: Vec<Value>,
}

impl Bin {
    /// Appends `data` as a new buffer view and returns its index.
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

/// A GLB container: 12-byte header, JSON chunk, BIN chunk (glTF 2.0 § 4.4).
fn glb(json: &Value, bin: &[u8]) -> Vec<u8> {
    let mut json_bytes = serde_json::to_vec(json).expect("json");
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let mut bin = bin.to_vec();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
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

fn png(width: u32, height: u32, raw: &[u8], color: image::ExtendedColorType) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(raw, width, height, color)
        .expect("PNG encode");
    out
}

/// One triangle's position, normal, UV and index views, appended to `bin`, and the accessors
/// that read them (positions 0, normals 1, UVs 2, indices 3).
fn triangle(bin: &mut Bin) -> Vec<Value> {
    let positions = bin.view(&f32_bytes(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]));
    let normals = bin.view(&f32_bytes(&[0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0]));
    let uvs = bin.view(&f32_bytes(&[0.0, 0.0, 1.0, 0.0, 0.0, 1.0]));
    let indices = bin.view(
        &[0u16, 1, 2]
            .iter()
            .flat_map(|i| i.to_le_bytes())
            .collect::<Vec<u8>>(),
    );
    vec![
        json!({"bufferView": positions, "componentType": 5126, "count": 3, "type": "VEC3",
               "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0]}),
        json!({"bufferView": normals, "componentType": 5126, "count": 3, "type": "VEC3"}),
        json!({"bufferView": uvs, "componentType": 5126, "count": 3, "type": "VEC2"}),
        json!({"bufferView": indices, "componentType": 5123, "count": 3, "type": "SCALAR"}),
    ]
}

/// A plain triangle GLB whose nodes are `nodes` (node `mesh_node` draws mesh 0) under one scene
/// rooted at `roots`.
fn triangle_glb(nodes: Vec<Value>, roots: Vec<usize>) -> Vec<u8> {
    let mut bin = Bin::default();
    let accessors = triangle(&mut bin);
    let json = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": roots}],
        "nodes": nodes,
        "meshes": [{"name": "Tri", "primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}, "indices": 3}]}],
        "accessors": accessors,
        "bufferViews": bin.views,
        "buffers": [{"byteLength": bin.bytes.len()}],
    });
    glb(&json, &bin.bytes)
}

/// `.meta` `scale` and `forward: Keep` reach the cooked vertex (spec 12 § 7, ADR-009).
/// The cook passes the parsed `forward` and the settings string; scale is read from that string.
#[test]
fn meta_scale_and_forward_keep_reach_the_cooked_vertex() {
    let dir = unique_temp_dir("gltf_meta_scale");
    let path = dir.join("unit.glb");
    std::fs::write(&path, triangle_glb(vec![json!({"mesh": 0})], vec![0])).expect("write glb");
    let settings = "(scale: 0.01, forward: Keep)";
    let parsed = regia_asset_pipeline::GltfImporterSettings::from_meta_string(Some(settings));
    assert_eq!(parsed.forward.to_forward(), Forward::Keep);
    assert!((parsed.import_scale() - 0.01).abs() < 1e-6, "{}", parsed.scale);
    let imported = import_gltf_path(
        &path,
        AssetId::new_random(),
        parsed.forward.to_forward(),
        "linux",
        Some(settings),
        TextureCookQuality::High,
    )
    .expect("import");
    let cpu = imported.primary_mesh.to_cpu_mesh().expect("unpack");
    assert!(
        cpu.positions
            .iter()
            .any(|p| { (p[0] - 0.01).abs() < 1e-5 && p[1].abs() < 1e-5 && p[2].abs() < 1e-5 }),
        "Keep and scale 0.01 should leave (1, 0, 0) at (0.01, 0, 0), got {:?}",
        cpu.positions
    );
    let converted = import_gltf_path(
        &path,
        AssetId::new_random(),
        Forward::Convert,
        "linux",
        None,
        TextureCookQuality::High,
    )
    .expect("default import");
    let cpu = converted.primary_mesh.to_cpu_mesh().expect("unpack");
    assert!(
        cpu.positions
            .iter()
            .any(|p| { (p[0] + 1.0).abs() < 1e-5 && p[1].abs() < 1e-5 && p[2].abs() < 1e-5 }),
        "Convert at scale 1 should turn (1, 0, 0) into (-1, 0, 0), got {:?}",
        cpu.positions
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The structured-fuzz fixture: one skinned triangle with a morph target, a textured material
/// (an embedded PNG read through a buffer view, for both its colour and normal maps), a two-joint
/// skin with inverse bind matrices and a rotation animation on the second joint.
fn rich_fixture() -> (Value, Vec<u8>) {
    let mut bin = Bin::default();
    let mut accessors = triangle(&mut bin);
    let joints = bin.view(&[0u8, 1, 0, 0, 0, 1, 0, 0, 1, 0, 0, 0]);
    let weights = bin.view(&f32_bytes(&[
        0.5, 0.5, 0.0, 0.0, 0.25, 0.75, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0,
    ]));
    let morph = bin.view(&f32_bytes(&[0.0, 0.0, 0.25, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]));
    let identity = [
        1.0f32, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ];
    let mut ibm = identity.to_vec();
    ibm.extend_from_slice(&identity);
    ibm[29] = -1.0; // second joint sits one metre up
    let ibms = bin.view(&f32_bytes(&ibm));
    let times = bin.view(&f32_bytes(&[0.0, 1.0]));
    let rotations = bin.view(&f32_bytes(&[
        0.0,
        0.0,
        0.0,
        1.0,
        0.0,
        std::f32::consts::FRAC_1_SQRT_2,
        0.0,
        std::f32::consts::FRAC_1_SQRT_2,
    ]));
    let pixels: Vec<u8> = (0..16u8).flat_map(|i| [i * 16, 255 - i * 16, 128, 255]).collect();
    let image = bin.view(&png(4, 4, &pixels, image::ExtendedColorType::Rgba8));
    accessors.extend([
        json!({"bufferView": joints, "componentType": 5121, "count": 3, "type": "VEC4"}),
        json!({"bufferView": weights, "componentType": 5126, "count": 3, "type": "VEC4"}),
        json!({"bufferView": morph, "componentType": 5126, "count": 3, "type": "VEC3",
               "min": [0.0, 0.0, 0.0], "max": [0.0, 0.0, 0.25]}),
        json!({"bufferView": ibms, "componentType": 5126, "count": 2, "type": "MAT4"}),
        json!({"bufferView": times, "componentType": 5126, "count": 2, "type": "SCALAR",
               "min": [0.0], "max": [1.0]}),
        json!({"bufferView": rotations, "componentType": 5126, "count": 2, "type": "VEC4"}),
    ]);
    let json = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0, 1]}],
        "nodes": [
            {"name": "Body", "mesh": 0, "skin": 0},
            {"name": "root", "children": [2]},
            {"name": "tip", "translation": [0.0, 1.0, 0.0]},
        ],
        "meshes": [{
            "name": "Body",
            "primitives": [{
                "attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2, "JOINTS_0": 4, "WEIGHTS_0": 5},
                "indices": 3,
                "material": 0,
                "targets": [{"POSITION": 6}],
            }],
            "weights": [0.0],
            "extras": {"targetNames": ["lift"]},
        }],
        "skins": [{"joints": [1, 2], "inverseBindMatrices": 7}],
        "animations": [{
            "name": "wave",
            "channels": [{"sampler": 0, "target": {"node": 2, "path": "rotation"}}],
            "samplers": [{"input": 8, "output": 9, "interpolation": "LINEAR"}],
        }],
        "materials": [{
            "name": "Skin",
            "pbrMetallicRoughness": {"baseColorTexture": {"index": 0}, "metallicFactor": 0.0, "roughnessFactor": 0.5},
            "normalTexture": {"index": 0},
        }],
        "textures": [{"source": 0}],
        "images": [{"bufferView": image, "mimeType": "image/png"}],
        "accessors": accessors,
        "bufferViews": bin.views,
        "buffers": [{"byteLength": bin.bytes.len()}],
    });
    (json, bin.bytes)
}

fn import_bytes(bytes: &[u8]) -> Result<regia_asset_pipeline::GltfImportResult, ImportError> {
    import_gltf_glb_bytes(
        bytes,
        AssetId::new_random(),
        Forward::Convert,
        "linux",
        TextureCookQuality::High,
    )
}

// ---------------------------------------------------------------------------------------------
// Image layouts in a `.gltf`
// ---------------------------------------------------------------------------------------------

/// Writes `name` beside the glTF with a `.meta` that keeps it RGBA8 (`compress: Some(false)`), so
/// the cooked image holds the widened pixels exactly.
fn write_uncompressed_image(dir: &std::path::Path, name: &str, bytes: &[u8]) {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write image");
    let mut meta = MetaSidecar::new("png", AssetId::new_random());
    meta.settings = Some("(compress: Some(false))".to_string());
    std::fs::write(meta_path_for(&path), meta.to_ron_string().expect("meta ron")).expect("write meta");
}

/// A grey PNG, a grey + alpha PNG and a 16-bit grey PNG referenced by a `.gltf` used to fail the
/// whole import ("unsupported glTF image format R8"): `gltf::import` keeps those layouts and only
/// RGB8/RGBA8 were converted. Each now widens like `image`'s `to_rgba8`.
#[test]
fn a_gltf_with_grey_and_16_bit_images_imports() {
    let dir = unique_temp_dir("gltf-grey-images");
    let mut bin = Bin::default();
    let accessors = triangle(&mut bin);
    std::fs::write(dir.join("tri.bin"), &bin.bytes).expect("write bin");

    write_uncompressed_image(
        &dir,
        "grey.png",
        &png(4, 4, &[100u8; 16], image::ExtendedColorType::L8),
    );
    let grey_alpha: Vec<u8> = [60u8, 200].repeat(16);
    write_uncompressed_image(
        &dir,
        "grey_alpha.png",
        &png(4, 4, &grey_alpha, image::ExtendedColorType::La8),
    );
    let deep: Vec<u8> = (0..16).flat_map(|_| 0x8000u16.to_ne_bytes()).collect();
    write_uncompressed_image(&dir, "deep.png", &png(4, 4, &deep, image::ExtendedColorType::L16));

    let json = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"name": "Tri", "mesh": 0}],
        "meshes": [{"name": "Tri", "primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}, "indices": 3, "material": 0}]}],
        "materials": [{
            "name": "Grey",
            "pbrMetallicRoughness": {"baseColorTexture": {"index": 0}},
            "emissiveTexture": {"index": 1},
            "occlusionTexture": {"index": 2},
        }],
        "textures": [{"source": 0}, {"source": 1}, {"source": 2}],
        "images": [{"uri": "grey.png"}, {"uri": "grey_alpha.png"}, {"uri": "deep.png"}],
        "accessors": accessors,
        "bufferViews": bin.views,
        "buffers": [{"byteLength": bin.bytes.len(), "uri": "tri.bin"}],
    });
    let source = dir.join("tri.gltf");
    std::fs::write(&source, serde_json::to_vec(&json).expect("json")).expect("write gltf");

    let imported = import_gltf_path(
        &source,
        AssetId::new_random(),
        Forward::Convert,
        "linux",
        None,
        TextureCookQuality::High,
    )
    .expect("grey, grey + alpha and 16-bit images import");

    let first_texel = |path: &str| -> [u8; 4] {
        let sub = imported
            .sub_assets
            .iter()
            .find(|s| s.path == path)
            .unwrap_or_else(|| panic!("{path} cooked"));
        assert_eq!(sub.type_path, "regia_asset::CookedImage", "{path} stays RGBA8");
        let image = CookedImage::from_postcard(&sub.payload).expect("cooked image decodes");
        assert_eq!((image.width, image.height), (4, 4));
        [image.rgba8[0], image.rgba8[1], image.rgba8[2], image.rgba8[3]]
    };
    assert_eq!(
        first_texel("texture/0"),
        [100, 100, 100, 255],
        "grey widens to (l, l, l, 255)"
    );
    assert_eq!(
        first_texel("texture/1"),
        [60, 60, 60, 200],
        "grey + alpha keeps its alpha"
    );
    assert_eq!(
        first_texel("texture/2"),
        [128, 128, 128, 255],
        "16-bit 0x8000 rounds to 128"
    );
    assert!(
        imported.sub_assets.iter().any(|s| s.path == "texture/orm/0"),
        "the 16-bit occlusion map packs into the ORM"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A metallic-roughness image whose `.meta` says `orm_channel_order: rma` must reach the material
/// swizzled: the material samples the packed `texture/orm/<mat>`, which was built from the raw
/// source pixels, so the swizzle (applied only to `texture/<n>`) never took effect.
#[test]
fn a_meta_channel_order_reaches_the_packed_orm() {
    let dir = unique_temp_dir("gltf-orm-swizzle");
    let mut bin = Bin::default();
    let accessors = triangle(&mut bin);
    std::fs::write(dir.join("tri.bin"), &bin.bytes).expect("write bin");
    // Source RMA: R roughness 10, G metallic 20, B AO 30.
    let path = dir.join("mr.png");
    std::fs::write(
        &path,
        png(
            2,
            2,
            &[10u8, 20, 30, 255].repeat(4),
            image::ExtendedColorType::Rgba8,
        ),
    )
    .expect("write image");
    let mut meta = MetaSidecar::new("png", AssetId::new_random());
    meta.settings = Some("(orm_channel_order: Some(rma))".to_string());
    std::fs::write(meta_path_for(&path), meta.to_ron_string().expect("meta ron")).expect("write meta");

    let json = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"name": "Tri", "mesh": 0}],
        "meshes": [{"name": "Tri", "primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}, "indices": 3, "material": 0}]}],
        "materials": [{
            "name": "Packed",
            "pbrMetallicRoughness": {"metallicRoughnessTexture": {"index": 0}},
            "occlusionTexture": {"index": 0},
        }],
        "textures": [{"source": 0}],
        "images": [{"uri": "mr.png"}],
        "accessors": accessors,
        "bufferViews": bin.views,
        "buffers": [{"byteLength": bin.bytes.len(), "uri": "tri.bin"}],
    });
    let source = dir.join("tri.gltf");
    std::fs::write(&source, serde_json::to_vec(&json).expect("json")).expect("write gltf");
    let imported = import_gltf_path(
        &source,
        AssetId::new_random(),
        Forward::Convert,
        "linux",
        None,
        TextureCookQuality::High,
    )
    .expect("imports");
    let orm = imported
        .sub_assets
        .iter()
        .find(|s| s.path == "texture/orm/0")
        .expect("the material packs an ORM");
    assert_eq!(orm.type_path, "regia_asset::CookedImage", "2x2 stays RGBA8");
    let image = CookedImage::from_postcard(&orm.payload).expect("cooked image decodes");
    assert_eq!(
        &image.rgba8[..4],
        &[30, 10, 20, 255],
        "occlusion R = source B, roughness G = source R, metallic B = source G"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Node hierarchies
// ---------------------------------------------------------------------------------------------

/// A node that is its own ancestor used to recurse until the stack overflowed, which aborts the
/// process (the cook, the editor) rather than failing one asset.
#[test]
fn node_cycles_are_errors_not_stack_overflows() {
    // 0 → 1 → 0, the mesh on node 1.
    let two_cycle = triangle_glb(
        vec![
            json!({"name": "a", "children": [1]}),
            json!({"name": "b", "mesh": 0, "children": [0]}),
        ],
        vec![0],
    );
    // A node that is its own child.
    let self_cycle = triangle_glb(vec![json!({"name": "a", "mesh": 0, "children": [0]})], vec![0]);
    for (what, bytes) in [("two-node cycle", two_cycle), ("self cycle", self_cycle)] {
        let mesh = assert_no_panic(what, || import_bytes(&bytes).map(|_| ()));
        assert!(mesh.is_err(), "{what} must be rejected");
        // The animation import walks the same nodes; it only has to return.
        let _ = assert_no_panic(what, || {
            import_gltf_animation(&bytes, &GltfAnimSettings::default()).map(|_| ())
        });
    }
}

/// sec-review-assets-agent (ADR-5869): glTF 2.0 § 3.5.3 nodes are disjoint strict trees. A node
/// under two parents was walked once per parent, which the importer bounded but the scene export,
/// plant export and editor walkers did not (a chain of shared pairs doubles per level). The checked
/// parse refuses it, so every walker gets a forest.
#[test]
fn a_node_with_two_parents_is_an_error() {
    let shared = triangle_glb(
        vec![
            json!({"name": "a", "children": [2]}),
            json!({"name": "b", "children": [2]}),
            json!({"name": "c", "mesh": 0}),
        ],
        vec![0, 1],
    );
    let result = assert_no_panic("shared child", || import_bytes(&shared).map(|_| ()));
    assert!(
        matches!(&result, Err(ImportError::Gltf(message)) if message.contains("more than one parent")),
        "expected the forest check, got {result:?}"
    );
    // The same child listed twice by one parent is two parents too.
    let twice = triangle_glb(
        vec![
            json!({"name": "a", "children": [1, 1]}),
            json!({"name": "c", "mesh": 0}),
        ],
        vec![0],
    );
    assert!(import_bytes(&twice).is_err());
}

/// sec-review-assets-agent (ADR-5869) finding: an accessor without a bufferView is `count` zeros
/// plus its sparse values, so nothing in the file backs `count`. A 2e9-element view-less sparse
/// POSITION made the readers collect ~24 GB from a few hundred bytes. The checked parse holds the
/// zero-filled elements of all view-less accessors to a budget tied to the buffers.
#[test]
fn a_viewless_accessor_cannot_declare_billions_of_elements() {
    let hostile = |position_is_viewless: bool| {
        let mut bin = Bin::default();
        let mut accessors = triangle(&mut bin);
        let sparse_index = bin.view(&0u32.to_le_bytes());
        let sparse_value = bin.view(&f32_bytes(&[1.0, 2.0, 3.0]));
        accessors.push(json!({
            "componentType": 5126, "count": 2_000_000_000u64, "type": "VEC3",
            "sparse": {"count": 1,
                       "indices": {"bufferView": sparse_index, "componentType": 5125},
                       "values": {"bufferView": sparse_value}}
        }));
        let position = if position_is_viewless { 4 } else { 0 };
        let json = json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0}],
            "meshes": [{"primitives": [{"attributes": {"POSITION": position}}]}],
            "accessors": accessors,
            "bufferViews": bin.views,
            "buffers": [{"byteLength": bin.bytes.len()}],
        });
        glb(&json, &bin.bytes)
    };
    // Unused: the parse refuses it before any reader runs (before the fix it imported).
    for bytes in [hostile(false), hostile(true)] {
        let result = assert_no_panic("view-less accessor", || import_bytes(&bytes).map(|_| ()));
        assert!(
            matches!(&result, Err(ImportError::Gltf(message)) if message.contains("without a buffer view")),
            "expected the view-less budget, got {result:?}"
        );
    }
}

/// A grey 8-bit PNG whose header claims `width × height` and whose image data is one empty stored
/// zlib block: the header reads, the pixels never decode.
fn png_header_claiming(width: u32, height: u32) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(
        &mut out,
        b"IDAT",
        &[0x78, 0x01, 0x01, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x01],
    );
    chunk(&mut out, b"IEND", &[]);
    out
}

/// A triangle GLB whose `images` are `count` entries naming one buffer view holding `png`.
fn glb_with_images(png: &[u8], count: usize) -> Vec<u8> {
    let mut bin = Bin::default();
    let accessors = triangle(&mut bin);
    let view = bin.view(png);
    let images: Vec<Value> = (0..count)
        .map(|_| json!({"bufferView": view, "mimeType": "image/png"}))
        .collect();
    let json = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2}, "indices": 3}]}],
        "accessors": accessors,
        "images": images,
        "bufferViews": bin.views,
        "buffers": [{"byteLength": bin.bytes.len()}],
    });
    glb(&json, &bin.bytes)
}

/// sec-review-assets-agent (ADR-5869): every glTF image was widened to RGBA8 and kept, with no
/// edge or total limit, and many image entries may name one small file: 32 references to a 500 KB
/// grey PNG of 22000² pixels were 64 GiB. The headers are read first now; an edge past
/// `MAX_GLTF_IMAGE_EDGE` or a sum past `MAX_GLTF_IMAGE_BYTES` is refused before any decode.
#[test]
fn image_headers_are_budgeted_before_any_decode() {
    use regia_asset_pipeline::{MAX_GLTF_IMAGE_BYTES, MAX_GLTF_IMAGE_EDGE};
    let image_error = |bytes: &[u8]| match assert_no_panic("image budget", || import_bytes(bytes).map(|_| ()))
    {
        Err(ImportError::Image(message)) => message,
        other => panic!("expected an image error, got {other:?}"),
    };
    let wide = image_error(&glb_with_images(
        &png_header_claiming(MAX_GLTF_IMAGE_EDGE + 1, 1),
        1,
    ));
    assert!(wide.contains("edge limit"), "{wide}");
    // 30000² RGBA8 is 3.6 GB: one is inside the budget (the empty pixels then fail to decode),
    // two are over it.
    let one = 30_000_u64 * 30_000 * 4;
    assert!(one <= MAX_GLTF_IMAGE_BYTES && 2 * one > MAX_GLTF_IMAGE_BYTES);
    let png = png_header_claiming(30_000, 30_000);
    let over = image_error(&glb_with_images(&png, 2));
    assert!(over.contains("more than"), "{over}");
    let inside = image_error(&glb_with_images(&png, 1));
    assert!(
        !inside.contains("more than"),
        "one image is inside the budget: {inside}"
    );
}

/// A chain of nodes thousands deep is an error at the importer's depth limit, not a stack
/// overflow (the mesh import and the node world pass of the animation import both walk it).
#[test]
fn a_node_chain_deeper_than_the_limit_is_an_error() {
    const DEPTH: usize = 5_000;
    let mut nodes: Vec<Value> = (0..DEPTH - 1)
        .map(|i| json!({"name": format!("n{i}"), "children": [i + 1]}))
        .collect();
    nodes.push(json!({"name": "leaf", "mesh": 0}));
    let bytes = triangle_glb(nodes, vec![0]);
    let result = assert_no_panic("deep chain", || import_bytes(&bytes).map(|_| ()));
    assert!(
        matches!(&result, Err(ImportError::Gltf(message)) if message.contains("deeper than")),
        "expected the depth-limit error, got {result:?}"
    );
    let anim = assert_no_panic("deep chain (animation)", || {
        import_gltf_animation(&bytes, &GltfAnimSettings::default()).map(|_| ())
    });
    assert!(
        anim.is_ok(),
        "no skins and no clips: nothing to reject ({anim:?})"
    );
}

/// A chain inside the limit (200 levels) imports, and the mesh carries every ancestor's
/// translation (0.5 m each, exact in `f32`).
#[test]
fn a_deep_chain_inside_the_limit_composes_every_transform() {
    const DEPTH: usize = 200;
    let mut nodes: Vec<Value> = (0..DEPTH - 1)
        .map(|i| json!({"name": format!("n{i}"), "translation": [0.0, 0.5, 0.0], "children": [i + 1]}))
        .collect();
    nodes.push(json!({"name": "leaf", "translation": [0.0, 0.5, 0.0], "mesh": 0}));
    let bytes = triangle_glb(nodes, vec![0]);
    let mesh = import_gltf(&bytes, Forward::Keep).expect("a 200-deep chain imports");
    let lowest = mesh.positions.iter().map(|p| p[1]).fold(f32::INFINITY, f32::min);
    assert_eq!(lowest, 0.5 * DEPTH as f32, "every level's translation is applied");
}

// ---------------------------------------------------------------------------------------------
// Fuzz
// ---------------------------------------------------------------------------------------------

/// JSON pointer of every value in `root` (objects and arrays included).
fn pointers(value: &Value, at: &mut String, out: &mut Vec<String>) {
    out.push(at.clone());
    let len = at.len();
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                at.push('/');
                at.push_str(&key.replace('~', "~0").replace('/', "~1"));
                pointers(child, at, out);
                at.truncate(len);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                at.push('/');
                at.push_str(&index.to_string());
                pointers(child, at, out);
                at.truncate(len);
            }
        }
        _ => {}
    }
}

/// Numbers a hostile file puts where counts, offsets, indices and factors go.
fn hostile_number(rng: &mut FuzzRng) -> Value {
    match rng.below(14) {
        0 => json!(0),
        1 => json!(1),
        2 => json!(2),
        3 => json!(3),
        4 => json!(7),
        5 => json!(255),
        6 => json!(65_535),
        7 => json!(u32::MAX),
        8 => json!(u64::MAX),
        9 => json!(-1),
        10 => json!(1e308),
        11 => json!(-1e308),
        12 => json!(0.5),
        _ => json!(9_007_199_254_740_993u64),
    }
}

/// One structural mutation of the glTF JSON.
fn mutate_gltf(root: &mut Value, rng: &mut FuzzRng) {
    // Rewire a node's children (cycles, self-parenting, shared children, out-of-range indices).
    if rng.chance(6) {
        let node = rng.below(3);
        let child = rng.below(4);
        if let Some(Value::Object(node)) = root.pointer_mut(&format!("/nodes/{node}")) {
            node.insert("children".to_string(), json!([child]));
        }
        return;
    }
    let mut all = Vec::new();
    pointers(root, &mut String::new(), &mut all);
    let pointer = all[rng.below(all.len())].clone();
    let replacement = match rng.below(6) {
        0 | 1 => hostile_number(rng),
        2 => Value::Null,
        3 => json!("data:application/octet-stream;base64,AAAA"),
        _ => {
            // Remove a key or an item, or duplicate an item.
            if let Some(target) = root.pointer_mut(&pointer) {
                match target {
                    Value::Object(map) if !map.is_empty() => {
                        let key = map.keys().nth(rng.below(map.len())).cloned();
                        if let Some(key) = key {
                            map.remove(&key);
                        }
                    }
                    Value::Array(items) if !items.is_empty() => {
                        let index = rng.below(items.len());
                        if rng.chance(2) {
                            items.remove(index);
                        } else {
                            let copy = items[index].clone();
                            items.push(copy);
                        }
                    }
                    other => *other = hostile_number(rng),
                }
            }
            return;
        }
    };
    if pointer.is_empty() {
        return;
    }
    if let Some(target) = root.pointer_mut(&pointer) {
        *target = replacement;
    }
}

/// Iterations of the structured fuzz.
const STRUCTURED: usize = 1_500;

#[test]
fn the_rich_fixture_imports_skin_morph_texture_and_clip() {
    let (json, bin) = rich_fixture();
    let bytes = glb(&json, &bin);
    let imported = import_bytes(&bytes).expect("fixture imports");
    let mesh = imported.primary_mesh.to_cpu_mesh().expect("primary mesh unpacks");
    assert!(mesh.skin.is_some(), "the skin cooks");
    assert_eq!(mesh.morph_targets.len(), 1, "the morph target cooks");
    assert_eq!(mesh.morph_targets[0].name, "lift");
    assert!(imported.sub_assets.iter().any(|s| s.path == "texture/0"));
    let anim = import_gltf_animation(&bytes, &GltfAnimSettings::default()).expect("clips import");
    assert_eq!(anim.skins.len(), 1, "the skin imports");
    assert!(!anim.skins[0].clips.is_empty(), "the rotation clip imports");
}

/// Every structured mutation of the rich fixture returns from both importers; enough of them
/// are rejected to show the mutations reach past the JSON parser.
#[test]
fn structured_gltf_mutations_never_panic() {
    let (fixture, bin) = rich_fixture();
    let mut rng = FuzzRng::new(0x5a55e7);
    let (mut mesh_errors, mut mesh_ok) = (0usize, 0usize);
    for iteration in 0..STRUCTURED {
        let mut json = fixture.clone();
        for _ in 0..rng.range(1, 3) {
            mutate_gltf(&mut json, &mut rng);
        }
        let bytes = glb(&json, &bin);
        let what = format!("structured mutation {iteration}");
        let mesh = assert_no_panic(&what, || import_bytes(&bytes).map(|_| ()));
        match mesh {
            Ok(()) => mesh_ok += 1,
            Err(error) => {
                assert!(!error.to_string().is_empty(), "{what}: empty error message");
                mesh_errors += 1;
            }
        }
        let _ = assert_no_panic(&what, || {
            import_gltf_animation(&bytes, &GltfAnimSettings::default()).map(|_| ())
        });
    }
    assert!(
        mesh_errors > 0,
        "no mutation was rejected: the fixture is not being corrupted"
    );
    assert!(
        mesh_ok > 0,
        "every mutation was rejected: the fixture never got past the parser"
    );
}

/// Byte-level mutation (truncation, bit flips, hostile length words) of the rich GLB.
#[test]
fn rich_glb_byte_mutations_never_panic() {
    let (json, bin) = rich_fixture();
    let fixture = glb(&json, &bin);
    fuzz_loader(&fixture, 0x6c7f, 1_000, |bytes| {
        import_gltf_glb_bytes(
            bytes,
            AssetId::new_random(),
            Forward::Convert,
            "linux",
            TextureCookQuality::High,
        )
    });
    fuzz_loader(&fixture, 0x6c80, 1_000, |bytes| {
        import_gltf_animation(bytes, &GltfAnimSettings::default())
    });
}

/// Pathhelper audit M1: the `gltf` crate opens a buffer or image URI as given, so a shared `.gltf`
/// named files outside the project and the cook put their bytes in the build. Each spelling is
/// refused before any read; a sibling folder inside the project stays reachable.
#[test]
fn a_uri_that_leaves_the_project_is_refused_before_it_is_read() {
    let top = unique_temp_dir("gltf_uri_confine");
    let project = top.join("project");
    let models = project.join("assets/models");
    std::fs::create_dir_all(&models).expect("dirs");
    std::fs::create_dir_all(project.join("assets/data")).expect("dirs");
    std::fs::write(project.join(regia_core::brand::MANIFEST_FILE), "").expect("manifest");
    std::fs::write(top.join("secret.bin"), [0u8; 4]).expect("secret");
    std::fs::write(project.join("assets/data/ok.bin"), [0u8; 4]).expect("sibling");
    let secret = top.join("secret.bin").display().to_string();
    let import = |uri: &str| {
        let path = models.join("m.gltf");
        let doc = json!({
            "asset": { "version": "2.0" },
            "buffers": [{ "uri": uri, "byteLength": 4 }],
        });
        std::fs::write(&path, doc.to_string()).expect("write");
        regia_asset_pipeline::gltf_materials(&path)
    };
    for bad in [
        "../../../secret.bin".to_string(),
        "..%2F..%2F..%2Fsecret.bin".to_string(),
        "nothing/../../../../secret.bin".to_string(),
        secret.clone(),
        format!("file://{secret}"),
    ] {
        let err = import(&bad).expect_err(&bad);
        assert!(
            matches!(&err, ImportError::Gltf(why) if why.contains("glTF URI")),
            "{bad}: {err}"
        );
    }
    assert!(
        import("../data/ok.bin").is_ok(),
        "a sibling folder inside the project"
    );
    let _ = std::fs::remove_dir_all(&top);
}

/// gltf 1.4's texcoord, joint, weight and index readers match on the component type and reach an
/// `unreachable!` for any other; glTF allows only a few per slot (§ 3.7.2.1). Found by the guided
/// fuzzer as a u32 `TEXCOORD_0` (`fuzz/findings/import.gltf/61f00769a64b311ce82f575db0028a57`,
/// ADR-2723). Each case fits its buffer view, so only the component-type check can refuse it.
#[test]
fn a_component_type_its_slot_does_not_allow_is_an_error_not_a_panic() {
    // (accessor in the rich fixture, its slot, a component type glTF does not allow there)
    for (accessor, slot, component) in [
        (2, "TEXCOORD_0", 5125), // u32
        (3, "indices", 5120),    // i8
        (4, "JOINTS_0", 5120),   // i8
        (5, "WEIGHTS_0", 5125),  // u32
    ] {
        let (mut json, bin) = rich_fixture();
        json["accessors"][accessor]["componentType"] = json!(component);
        let err = import_bytes(&glb(&json, &bin)).expect_err(slot);
        assert!(
            err.to_string().contains("component type its use does not allow"),
            "{slot}: {err}"
        );
    }
    let (json, bin) = rich_fixture();
    assert!(import_bytes(&glb(&json, &bin)).is_ok(), "the unchanged fixture");
}

/// Declares `KHR_mesh_quantization` the way the extension says a file must (used and required).
fn declare_quantization(json: &mut Value) {
    json["extensionsUsed"] = json!(["KHR_mesh_quantization"]);
    json["extensionsRequired"] = json!(["KHR_mesh_quantization"]);
}

/// Appends `data` as a new view of the rich fixture's buffer; returns the view's index.
fn add_view(json: &mut Value, bin: &mut Vec<u8>, data: &[u8]) -> usize {
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let offset = bin.len();
    bin.extend_from_slice(data);
    let views = json["bufferViews"].as_array_mut().expect("views");
    views.push(json!({"buffer": 0, "byteOffset": offset, "byteLength": data.len()}));
    let view = views.len() - 1;
    json["buffers"][0]["byteLength"] = json!(bin.len());
    view
}

/// Appends `data` as a new view of the rich fixture's buffer, and an accessor over it; returns the
/// accessor's index.
fn add_accessor(json: &mut Value, bin: &mut Vec<u8>, data: &[u8], accessor: Value) -> usize {
    let view = add_view(json, bin, data);
    let mut accessor = accessor;
    accessor["bufferView"] = json!(view);
    let accessors = json["accessors"].as_array_mut().expect("accessors");
    accessors.push(accessor);
    accessors.len() - 1
}

/// The four signed `TEXCOORD_0` forms of the rich fixture (`BYTE`/`SHORT`, normalized or not).
fn signed_texcoord_forms() -> Vec<(u32, bool, &'static str)> {
    vec![
        (5120, false, "BYTE"),
        (5120, true, "BYTE normalized"),
        (5122, false, "SHORT"),
        (5122, true, "SHORT normalized"),
    ]
}

/// ADR-5958: a signed `TEXCOORD_n` is a `KHR_mesh_quantization` form. Without that extension declared
/// it stays refused by the component-type check (ADR-2723); with it, the file imports.
#[test]
fn a_signed_texcoord_needs_the_quantization_extension() {
    for (component, normalized, name) in signed_texcoord_forms() {
        let (mut json, bin) = rich_fixture();
        json["accessors"][2]["componentType"] = json!(component);
        json["accessors"][2]["normalized"] = json!(normalized);
        let err = import_bytes(&glb(&json, &bin)).expect_err(name);
        assert!(
            err.to_string().contains("component type its use does not allow"),
            "{name} without the extension: {err}"
        );
        // Named somewhere other than the file's extension lists is not a declaration.
        json["asset"]["extras"] = json!({"extensionsUsed": ["KHR_mesh_quantization"]});
        let err = import_bytes(&glb(&json, &bin)).expect_err(name);
        assert!(
            err.to_string().contains("component type its use does not allow"),
            "{name} named only in extras: {err}"
        );
        declare_quantization(&mut json);
        if let Err(err) = import_bytes(&glb(&json, &bin)) {
            panic!("{name} with the extension: {err}");
        }
    }
}

/// ADR-5958: the extension widens `TEXCOORD_n` only. Every other use keeps the component types core
/// glTF allows, with the extension declared too: signed colors (the extension's table has no
/// `COLOR_n`), signed or 32-bit joints, weights and indices, and a 32-bit `TEXCOORD_0` (the fuzzer's
/// finding).
#[test]
fn the_quantization_extension_widens_no_other_use() {
    // (accessor in the rich fixture, its slot, component type, normalized)
    let mut cases: Vec<(Option<usize>, &str, u32, bool)> = vec![
        (Some(2), "TEXCOORD_0", 5125, false),
        (Some(3), "indices", 5120, false),
        (Some(3), "indices", 5122, false),
        (Some(4), "JOINTS_0", 5120, false),
        (Some(4), "JOINTS_0", 5122, false),
        (Some(4), "JOINTS_0", 5125, false),
        (Some(5), "WEIGHTS_0", 5120, true),
        (Some(5), "WEIGHTS_0", 5122, true),
        (Some(5), "WEIGHTS_0", 5125, false),
    ];
    // A new COLOR_0 accessor (None) in each signed or 32-bit form.
    for (component, normalized) in [
        (5120, true),
        (5120, false),
        (5122, true),
        (5122, false),
        (5125, false),
    ] {
        cases.push((None, "COLOR_0", component, normalized));
    }
    for (accessor, slot, component, normalized) in cases {
        let (mut json, mut bin) = rich_fixture();
        declare_quantization(&mut json);
        let index = match accessor {
            Some(index) => {
                // A view that fits three elements of any component type tried (up to 4 x 4 bytes
                // each), so the range check passes and only the component-type check can refuse.
                let view = add_view(&mut json, &mut bin, &[0; 48]);
                json["accessors"][index]["bufferView"] = json!(view);
                index
            }
            None => {
                // Three VEC4 elements, 16 bytes each: fits every component type tried.
                let color = add_accessor(
                    &mut json,
                    &mut bin,
                    &[0x40; 48],
                    json!({"componentType": 5121, "normalized": true, "count": 3, "type": "VEC4"}),
                );
                json["meshes"][0]["primitives"][0]["attributes"]["COLOR_0"] = json!(color);
                assert!(
                    import_bytes(&glb(&json, &bin)).is_ok(),
                    "the fixture with an UNSIGNED_BYTE color"
                );
                color
            }
        };
        json["accessors"][index]["componentType"] = json!(component);
        json["accessors"][index]["normalized"] = json!(normalized);
        let what = format!("{slot} as {component} (normalized {normalized})");
        let err = import_bytes(&glb(&json, &bin)).expect_err(&what);
        assert!(
            err.to_string().contains("component type its use does not allow"),
            "{what}: {err}"
        );
    }
}

/// ADR-5958: a signed `TEXCOORD_n` is accepted only because the extension's decode runs before any
/// reader: what [`regia_asset_pipeline::open_gltf`] hands a tool holds float UVs only, so gltf's
/// texcoord reader never meets a component type it does not take (its `unreachable!`, ADR-2723).
#[test]
fn an_opened_quantized_file_hands_out_float_texcoords_only() {
    let dir = unique_temp_dir("gltf_quantized_open");
    for (component, normalized, name) in signed_texcoord_forms() {
        let (mut json, bin) = rich_fixture();
        json["accessors"][2]["componentType"] = json!(component);
        json["accessors"][2]["normalized"] = json!(normalized);
        declare_quantization(&mut json);
        let path = dir.join(format!("{component}_{normalized}.glb"));
        std::fs::write(&path, glb(&json, &bin)).expect("write glb");
        let opened = regia_asset_pipeline::open_gltf(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        let document = &opened.document;
        for mesh in document.meshes() {
            for primitive in mesh.primitives() {
                for (semantic, accessor) in primitive.attributes() {
                    if matches!(semantic, gltf::Semantic::TexCoords(_)) {
                        assert_eq!(
                            accessor.data_type(),
                            gltf::accessor::DataType::F32,
                            "{name}: {semantic:?} reaches the reader as {:?}",
                            accessor.data_type()
                        );
                    }
                }
                let reader = primitive.reader(|b| opened.buffers.get(b.index()).map(|d| d.0.as_slice()));
                let uvs: Vec<[f32; 2]> = reader
                    .read_tex_coords(0)
                    .expect("TEXCOORD_0")
                    .into_f32()
                    .collect();
                assert_eq!(uvs.len(), 3, "{name}");
                assert!(uvs.iter().flatten().all(|v| v.is_finite()), "{name}: {uvs:?}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `docs/security/threat-model.md`: a node listed twice under each parent doubles the walk at every
/// level, inside the depth limit. 21 levels are 2^21 visits, twice the visit limit: enough to show
/// the limit, and small enough that an import without it finishes and fails this test instead of
/// hanging (40 levels would be 2^40).
#[test]
fn a_node_graph_that_grows_the_walk_exponentially_stops_at_the_visit_limit() {
    const LEVELS: usize = 21;
    // No mesh under the walk: the limit counts visits, and importing a mesh per leaf visit would only
    // make the test slow.
    let mut nodes: Vec<Value> = (0..LEVELS - 1)
        .map(|i| json!({"name": format!("n{i}"), "children": [i + 1, i + 1]}))
        .collect();
    nodes.push(json!({"name": "leaf"}));
    let bytes = triangle_glb(nodes, vec![0]);
    let result = assert_no_panic("exponential walk", || import_bytes(&bytes).map(|_| ()));
    assert!(
        matches!(&result, Err(ImportError::Gltf(message)) if message.contains("visits") || message.contains("nodes")),
        "expected the visit-limit error, got {result:?}"
    );
}
