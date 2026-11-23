// SPDX-License-Identifier: MPL-2.0
//! Cooked materials that reference textures round-trip through postcard and load
//! from a pak. A version-1 cache is recooked instead of failing the cook.

use std::collections::BTreeMap;
use std::path::PathBuf;

use image::ImageEncoder;
use regia_asset::{
    AssetManifest, AssetServer, AssetServerSettings, AssetSource, COOKED_MATERIAL_VERSION, MaterialAssetRon,
    TextureCookUsage, TextureSlot, cooked_cache_dir, cooked_file_name, decode_cooked_file,
    encode_cooked_file, load_material_bytes, pack_importer_version,
};
use regia_asset_pipeline::{AssetDatabase, CookOptions, cook_project, host_platform, pack_cooked_directory};

struct TempProject(PathBuf);

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_project(label: &str) -> TempProject {
    let path = std::env::temp_dir().join(format!("regia_mat_{label}_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(path.join("assets")).expect("project dir");
    TempProject(path)
}

fn solid_png(pixel: [u8; 4]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(4 * 4 * 4);
    for _ in 0..16 {
        raw.extend_from_slice(&pixel);
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&raw, 4, 4, image::ExtendedColorType::Rgba8)
        .expect("png");
    out
}

struct Span {
    offset: usize,
    len: usize,
}

fn place(bin: &mut Vec<u8>, bytes: &[u8]) -> Span {
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let offset = bin.len();
    bin.extend_from_slice(bytes);
    Span {
        offset,
        len: bytes.len(),
    }
}

fn triangle_positions() -> Vec<u8> {
    let mut pos = Vec::new();
    for p in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
        for component in p {
            pos.extend_from_slice(&component.to_le_bytes());
        }
    }
    pos
}

fn triangle_indices() -> Vec<u8> {
    let mut idx = Vec::new();
    for i in [0u32, 1, 2] {
        idx.extend_from_slice(&i.to_le_bytes());
    }
    idx
}

/// One triangle plus base-colour, normal, metallic–roughness and occlusion images.
fn hero_glb() -> Vec<u8> {
    let images = [
        solid_png([200, 30, 20, 255]),
        solid_png([128, 128, 255, 255]),
        solid_png([0, 180, 40, 255]),
        solid_png([90, 0, 0, 255]),
    ];
    let mut bin = Vec::new();
    let pos = place(&mut bin, &triangle_positions());
    let idx = place(&mut bin, &triangle_indices());
    let placed: Vec<Span> = images.iter().map(|bytes| place(&mut bin, bytes)).collect();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }

    let image_views = placed
        .iter()
        .map(|span| {
            format!(
                r#"{{"buffer":0,"byteOffset":{},"byteLength":{}}}"#,
                span.offset, span.len
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let views = format!(
        r#"{{"buffer":0,"byteOffset":{},"byteLength":{},"byteStride":12,"target":34962}},{{"buffer":0,"byteOffset":{},"byteLength":{},"target":34963}},{image_views}"#,
        pos.offset, pos.len, idx.offset, idx.len,
    );
    let json = format!(
        r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":[{{"mesh":0,"name":"Hero"}}],"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1,"mode":4,"material":0}}]}}],"materials":[{{"name":"Hero","pbrMetallicRoughness":{{"baseColorFactor":[0.8,0.2,0.1,1.0],"metallicFactor":0.0,"roughnessFactor":0.6,"baseColorTexture":{{"index":0}},"metallicRoughnessTexture":{{"index":2}}}},"normalTexture":{{"index":1}},"occlusionTexture":{{"index":3}}}}],"textures":[{{"source":0}},{{"source":1}},{{"source":2}},{{"source":3}}],"images":[{{"bufferView":2,"mimeType":"image/png"}},{{"bufferView":3,"mimeType":"image/png"}},{{"bufferView":4,"mimeType":"image/png"}},{{"bufferView":5,"mimeType":"image/png"}}],"accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0.0,0.0,0.0],"max":[1.0,1.0,0.0]}},{{"bufferView":1,"componentType":5125,"count":3,"type":"SCALAR"}}],"bufferViews":[{views}],"buffers":[{{"byteLength":{}}}]}}"#,
        bin.len(),
    );
    let mut json_bytes = json.into_bytes();
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let mut glb = Vec::new();
    glb.extend_from_slice(&0x4654_6C67u32.to_le_bytes());
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total as u32).to_le_bytes());
    glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json_bytes);
    glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"BIN\0");
    glb.extend_from_slice(&bin);
    glb
}

fn cook_options(project: &TempProject) -> CookOptions {
    CookOptions {
        incremental: true,
        ..CookOptions::for_host(project.0.clone())
    }
}

#[test]
fn cooked_gltf_material_textures_resolve_from_pak() {
    let project = temp_project("hero");
    std::fs::write(project.0.join("assets/hero.glb"), hero_glb()).expect("write glb");
    let options = cook_options(&project);
    let report = cook_project(options).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(report.cooked >= 1, "expected the glb to cook");

    let cache = cooked_cache_dir(&project.0, host_platform());
    let content = project.0.join("content");
    pack_cooked_directory(&cache, &content, &BTreeMap::new(), true).expect("pak");

    let mut server = AssetServer::open(AssetServerSettings {
        project_root: project.0.clone(),
        platform: host_platform().to_string(),
        content_dir: Some(content),
    })
    .expect("open");
    assert!(server.has_paks());

    let material_path = "assets/hero.glb#material/Hero";
    let material_id = server.id_for_path(material_path).expect("material id");
    let base_id = server
        .id_for_path("assets/hero.glb#texture/0")
        .expect("base color id");
    let normal_id = server
        .id_for_path("assets/hero.glb#texture/1")
        .expect("normal id");
    let orm_id = server
        .id_for_path("assets/hero.glb#texture/orm/0")
        .expect("orm id");
    let raw_mr = server
        .id_for_path("assets/hero.glb#texture/2")
        .expect("source mr id");
    assert_ne!(orm_id, raw_mr, "packed ORM is its own asset");

    let handle = server.load_material_path(material_path).expect("load material");
    assert_eq!(
        server.source_of(material_id),
        AssetSource::Pak("base".to_string())
    );
    let material = server.materials().get(&handle).expect("stored material");
    assert_eq!(material.version, COOKED_MATERIAL_VERSION);
    assert!(matches!(
        material.authored.base.base_color_map,
        Some(TextureSlot::Asset(id)) if id == base_id
    ));
    assert!(matches!(
        material.authored.base.normal_map,
        Some(TextureSlot::Asset(id)) if id == normal_id
    ));
    assert!(matches!(
        material.authored.base.orm_map,
        Some(TextureSlot::Asset(id)) if id == orm_id
    ));

    let base = server.load_texture(base_id).expect("base color texture");
    let normal = server.load_texture(normal_id).expect("normal texture");
    let orm = server.load_texture(orm_id).expect("orm texture");
    let base_tex = server.textures().get(&base).expect("base stored");
    let normal_tex = server.textures().get(&normal).expect("normal stored");
    let orm_tex = server.textures().get(&orm).expect("orm stored");
    assert_eq!(
        (base_tex.width, base_tex.height, base_tex.usage),
        (4, 4, TextureCookUsage::ColorSrgb)
    );
    assert_eq!(
        (normal_tex.width, normal_tex.height, normal_tex.usage),
        (4, 4, TextureCookUsage::NormalMap)
    );
    assert_eq!(
        (orm_tex.width, orm_tex.height, orm_tex.usage),
        (4, 4, TextureCookUsage::ColorLinear)
    );
    assert!(!base_tex.ktx2.is_empty() && !normal_tex.ktx2.is_empty() && !orm_tex.ktx2.is_empty());
    assert_eq!(server.source_of(base_id), AssetSource::Pak("base".to_string()));
    assert_eq!(server.source_of(normal_id), AssetSource::Pak("base".to_string()));
    assert_eq!(server.source_of(orm_id), AssetSource::Pak("base".to_string()));

    let mut cache_server = AssetServer::open(AssetServerSettings {
        project_root: project.0.clone(),
        platform: host_platform().to_string(),
        content_dir: None,
    })
    .expect("cache server");
    let cached = cache_server.load_material(material_id).expect("cache load");
    assert_eq!(cache_server.source_of(material_id), AssetSource::DevCache);
    let cached_mat = cache_server.materials().get(&cached).expect("cached material");
    assert_eq!(cached_mat.authored.base.orm_map, Some(TextureSlot::Asset(orm_id)));
}

#[test]
fn stale_cooked_material_recooks_instead_of_failing() {
    let project = temp_project("stale");
    let mut authored = MaterialAssetRon::default_standard();
    authored.name = Some("Paint".to_string());
    authored.base.base_color_map = Some(TextureSlot::Path("textures/paint.png".to_string()));
    let text = authored.to_ron_string().expect("ron");
    std::fs::write(project.0.join("assets/paint.material.ron"), text).expect("write material");

    let options = cook_options(&project);
    let first = cook_project(options.clone()).expect("first cook");
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert_eq!(first.cooked, 1);

    let second = cook_project(options.clone()).expect("cached cook");
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    assert_eq!(second.cooked, 0);
    assert_eq!(second.skipped, 1);

    let cache = cooked_cache_dir(&project.0, host_platform());
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest io")
        .expect("manifest");
    let id = manifest.id_for_path("assets/paint.material.ron").expect("id");
    tamper_material_payload(&cache.join(cooked_file_name(id)));

    let third = cook_project(options.clone()).expect("stale cook");
    assert!(
        third.errors.is_empty(),
        "a stale cooked material must recook, not fail the cook: {:?}",
        third.errors
    );
    assert_eq!(third.cooked, 1, "stale payload must be rewritten");
    assert_eq!(third.skipped, 0);

    let mut server = AssetServer::open(AssetServerSettings {
        project_root: project.0.clone(),
        platform: host_platform().to_string(),
        content_dir: None,
    })
    .expect("open");
    let handle = server
        .load_material_path("assets/paint.material.ron")
        .expect("load");
    assert_eq!(server.source_of(id), AssetSource::DevCache);
    let loaded = server.materials().get(&handle).expect("stored");
    assert_eq!(loaded.version, COOKED_MATERIAL_VERSION);
    assert!(matches!(
        loaded.authored.base.base_color_map,
        Some(TextureSlot::Path(ref path)) if path == "textures/paint.png"
    ));

    let fourth = cook_project(options.clone()).expect("settled");
    assert_eq!(fourth.cooked, 0);
    assert_eq!(fourth.skipped, 1);

    let mut db = AssetDatabase::open(&project.0).expect("db");
    let row = db.entries.get_mut("assets/paint.material.ron").expect("row");
    row.cache_key = [0u8; 32];
    row.importer_version = pack_importer_version(1, 0, 0);
    db.save(&project.0).expect("save db");
    tamper_material_payload(&cache.join(cooked_file_name(id)));

    let fifth = cook_project(options).expect("old cache key");
    assert!(fifth.errors.is_empty(), "{:?}", fifth.errors);
    assert_eq!(fifth.cooked, 1, "a 1.0.0 cache key must miss and recook");
    let reloaded = load_material_bytes(&std::fs::read(cache.join(cooked_file_name(id))).expect("bytes"))
        .expect("reloaded file");
    assert_eq!(reloaded.1.version, COOKED_MATERIAL_VERSION);
    // 1.5.0 (ADR-3320): the cooked header lists the texture-slot asset ids.
    assert_eq!(reloaded.0.importer_version, pack_importer_version(1, 5, 0));
}

/// Replace a cooked material payload with a version-1 integer, the leading
/// value of every pre-tag cache (`format_version` was the first field).
fn tamper_material_payload(path: &std::path::Path) {
    let bytes = std::fs::read(path).expect("read cooked");
    let (mut header, _) = decode_cooked_file(&bytes).expect("header");
    let stale = postcard::to_allocvec(&1u32).expect("version byte");
    header.payload_len = stale.len() as u64;
    let file = encode_cooked_file(&header, &stale).expect("encode stale");
    std::fs::write(path, file).expect("write stale");
    assert!(load_material_bytes(&std::fs::read(path).expect("reread")).is_err());
}
