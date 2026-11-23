// SPDX-License-Identifier: MPL-2.0
//! Counted built-in loads read like typed loads (ADR-7284, spec 12 § 3): a counted texture from a
//! mapped pak keeps a view of its KTX2 bytes (no copy), counted loads record their load forms in
//! `AssetServer::load_forms`, and a counted texture whose cooked file changed kind (KTX2 to RGBA8
//! image, and back) takes the new kind on hot reload. Every test steps the server by calls
//! (`wait_for_loads`, `reload_id`); no clock.

use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use regia_asset::{
    AssetId, AssetServer, AssetServerSettings, AssetSource, AssetTypeTag, COOKED_FORMAT_VERSION,
    CookedHeader, CookedImage, CookedTexture, CpuMesh, FPAK_BLOB_ALIGNMENT, FPAK_VERSION, LoadState,
    LoadedKind, ManifestEntry, PakEntry, PakToc, PrepareAsset, TextureCookUsage, cooked_cache_dir,
    cooked_file_name, encode_cooked_file, encode_pak_header, pack_importer_version,
};

fn cooked(id: AssetId, tag: AssetTypeTag, payload: &[u8]) -> Vec<u8> {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: tag.0,
        asset_id: id,
        importer_version: pack_importer_version(1, 0, 0),
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    encode_cooked_file(&header, payload).expect("encode")
}

fn texture_file(id: AssetId) -> Vec<u8> {
    let texture = CookedTexture {
        width: 4,
        height: 4,
        usage: TextureCookUsage::ColorSrgb,
        ktx2: minimal_ktx2().into(),
    };
    cooked(
        id,
        AssetTypeTag::texture(),
        &texture.to_postcard().expect("postcard"),
    )
}

fn image_file(id: AssetId) -> Vec<u8> {
    let image = CookedImage {
        width: 2,
        height: 2,
        rgba8: vec![7; 16],
    };
    cooked(id, AssetTypeTag::image(), &image.to_postcard().expect("postcard"))
}

fn mesh_file(id: AssetId) -> Vec<u8> {
    let mesh = CpuMesh {
        positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        normals: vec![[0.0, 0.0, 1.0]; 3],
        uvs: vec![[0.0, 0.0]; 3],
        indices: vec![0, 1, 2],
        ..Default::default()
    };
    cooked(
        id,
        AssetTypeTag::mesh_legacy_cpu(),
        &mesh.to_postcard().expect("postcard"),
    )
}

/// An uncompressed, unhashed pak of `entries` (id, path, cooked bytes): every entry is served from
/// the mapping once the pak mounts.
fn write_pak(path: &Path, entries: &[(AssetId, &str, Vec<u8>)]) {
    let mut data = vec![0u8; 60];
    let mut toc_entries = Vec::new();
    let mut manifest = Vec::new();
    let mut sorted: Vec<_> = entries.iter().collect();
    sorted.sort_by_key(|(id, _, _)| *id);
    for (id, asset_path, payload) in sorted {
        let rem = data.len() % FPAK_BLOB_ALIGNMENT as usize;
        if rem != 0 {
            data.resize(data.len() + (FPAK_BLOB_ALIGNMENT as usize - rem), 0);
        }
        let offset = data.len() as u64;
        data.extend_from_slice(payload);
        toc_entries.push(PakEntry {
            id: *id,
            type_tag: regia_core::id::TypePathHash(0),
            offset,
            size: payload.len() as u64,
            compressed_size: payload.len() as u64,
            block_table_offset: 0,
        });
        manifest.push(ManifestEntry {
            path: (*asset_path).to_owned(),
            id: *id,
            type_path: String::new(),
        });
    }
    let toc_offset = data.len() as u64;
    let toc = PakToc {
        entries: toc_entries,
        manifest,
        dependencies: Vec::new(),
    };
    let toc_bytes = postcard::to_allocvec(&toc).expect("toc");
    data.extend_from_slice(&toc_bytes);
    // No content hash: an unhashed pak serves views of its mapping as soon as it mounts (a hashed
    // one copies until the background whole-file check passes, `pak_verify`).
    let header = encode_pak_header(FPAK_VERSION, 0, toc_offset, toc_bytes.len() as u64, &[0u8; 32]);
    data[..60].copy_from_slice(&header);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, data).expect("write pak");
}

fn write_atomic(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

fn open(root: &Path, content: Option<&Path>) -> AssetServer {
    AssetServer::open(AssetServerSettings {
        project_root: root.to_path_buf(),
        platform: "linux".into(),
        content_dir: content.map(Path::to_path_buf),
    })
    .expect("open")
}

/// A minimal valid KTX2 blob (one 4×4 BC7 block), so `CookedTexture::validate` accepts it.
fn minimal_ktx2() -> Vec<u8> {
    use ktx2::dfd::{Basic, Block};
    use ktx2::{Format, Header, Index, LevelIndex};
    let (basic, type_size) = Basic::from_format(Format::BC7_SRGB_BLOCK).expect("dfd");
    let dfd_body = Block::Basic(basic).to_vec();
    let dfd_total_len = 4u32 + dfd_body.len() as u32;
    let dfd_offset = Header::LENGTH + LevelIndex::LENGTH;
    let kvd_offset = dfd_offset + dfd_total_len as usize;
    let payload = vec![0u8; 16];
    let level_index = LevelIndex {
        byte_offset: kvd_offset as u64,
        byte_length: payload.len() as u64,
        uncompressed_byte_length: payload.len() as u64,
    };
    let header = Header {
        format: Some(Format::BC7_SRGB_BLOCK),
        type_size,
        pixel_width: 4,
        pixel_height: 4,
        pixel_depth: 0,
        layer_count: 0,
        face_count: 1,
        level_count: 1,
        supercompression_scheme: None,
        index: Index {
            dfd_byte_offset: dfd_offset as u32,
            dfd_byte_length: dfd_total_len,
            kvd_byte_offset: kvd_offset as u32,
            kvd_byte_length: 0,
            sgd_byte_offset: 0,
            sgd_byte_length: 0,
        },
    };
    let mut out = Vec::new();
    out.extend_from_slice(&header.as_bytes());
    out.extend_from_slice(&level_index.as_bytes());
    out.extend_from_slice(&dfd_total_len.to_le_bytes());
    out.extend_from_slice(&dfd_body);
    out.extend_from_slice(&payload);
    out
}

/// Item 2: a counted texture load from a mapped pak keeps a view of the pak's KTX2 bytes, as the
/// typed `load_texture` does; no heap copy of the payload is made.
#[test]
fn a_counted_texture_from_a_mapped_pak_is_a_view_of_the_mapping() {
    let root = regia_test_utils::unique_temp_dir("counted-pak-view");
    let content = root.join("content");
    std::fs::create_dir_all(cooked_cache_dir(&root, "linux")).expect("cache");
    let id = AssetId::new_random();
    write_pak(
        &content.join("base.fpak"),
        &[(id, "textures/t.ktx2", texture_file(id))],
    );
    let mut server = open(&root, Some(&content));
    assert_eq!(server.zero_copy_pak_count(), 1, "the pak serves views");

    let handle = server.load::<CookedTexture>(id).expect("load");
    server.wait_for_loads();
    assert_eq!(server.state(id), LoadState::Loaded);
    assert_eq!(server.source_of(id), AssetSource::Pak("base".into()));
    let texture = server.get(&handle).expect("texture");
    assert_eq!(texture.ktx2.as_ref(), minimal_ktx2().as_slice());
    assert!(
        texture.ktx2.is_mapped(),
        "the counted texture's KTX2 bytes are a view of the mapped pak, not a heap copy"
    );
    drop(handle);
    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// Item 3: counted loads record their load forms (mesh form, KTX2 texture, RGBA8 image, pak or
/// cache read) in `load_forms`, exactly as the typed loads of the same files do.
#[test]
fn counted_loads_record_their_load_forms_like_typed_loads() {
    let root = regia_test_utils::unique_temp_dir("counted-load-forms");
    let content = root.join("content");
    let cache = cooked_cache_dir(&root, "linux");
    std::fs::create_dir_all(&cache).expect("cache");
    let (mesh, texture, image, cached) = (
        AssetId::new_random(),
        AssetId::new_random(),
        AssetId::new_random(),
        AssetId::new_random(),
    );
    write_pak(
        &content.join("base.fpak"),
        &[
            (mesh, "meshes/m.mesh", mesh_file(mesh)),
            (texture, "textures/t.ktx2", texture_file(texture)),
            (image, "textures/i.png", image_file(image)),
        ],
    );
    write_atomic(&cache.join(cooked_file_name(cached)), &image_file(cached));

    // The typed loads of the same files: the reference counts.
    let mut typed = open(&root, Some(&content));
    typed.load_mesh(mesh).expect("mesh");
    typed.load_texture(texture).expect("texture");
    typed.load_image(image).expect("image");
    typed.load_image(cached).expect("cached image");
    let want = typed.load_forms();
    assert_eq!(
        (
            want.legacy_meshes,
            want.textures,
            want.images_rgba8,
            want.pak_loads,
            want.cache_loads
        ),
        (1, 1, 2, 3, 1)
    );

    let mut server = open(&root, Some(&content));
    let handles = (
        server.load::<CpuMesh>(mesh).expect("mesh"),
        server.load::<CookedTexture>(texture).expect("texture"),
        server.load::<CookedImage>(image).expect("image"),
        server.load::<CookedImage>(cached).expect("cached image"),
    );
    server.wait_for_loads();
    for id in [mesh, texture, image, cached] {
        assert_eq!(server.state(id), LoadState::Loaded, "{id}");
    }
    assert_eq!(server.load_forms(), want, "counted loads count like typed loads");
    drop(handles);
    let _ = std::fs::remove_dir_all(&root);
}

/// Item 5: a counted texture whose cooked file becomes an image still keeps the texture value
/// when the image kind's [`PrepareAsset`] step rejects the decoded value on reload: the kind
/// switch is skipped, the old value and its GPU copy survive, and the reload reports the error.
struct RejectImages;

impl PrepareAsset for RejectImages {
    type Asset = CookedImage;

    fn prepare(&self, _id: AssetId, _asset: &CookedImage) -> Result<u64, String> {
        Err("image rejected by test preparer".to_string())
    }
}

#[test]
fn a_kind_switch_whose_prepare_fails_keeps_the_previous_value() {
    let root = regia_test_utils::unique_temp_dir("counted-kind-prepare-fails");
    let cache = cooked_cache_dir(&root, "linux");
    std::fs::create_dir_all(&cache).expect("cache");
    let id = AssetId::new_random();
    let path = cache.join(cooked_file_name(id));
    write_atomic(&path, &texture_file(id));
    let mut server = open(&root, None);
    let freed = Arc::new(Freed::default());
    server.register_prepare(TextureUpload(Arc::clone(&freed)));
    server.register_prepare(RejectImages);

    let texture = server.load::<CookedTexture>(id).expect("load");
    server.wait_for_loads();
    assert!(server.get(&texture).is_some());

    // KTX2 -> PNG, but the image kind's prepare step rejects the decoded value.
    write_atomic(&path, &image_file(id));
    assert!(
        server.reload_id(id).is_err(),
        "a reload whose prepare fails reports the error"
    );
    assert!(
        server.get(&texture).is_some(),
        "a failed kind switch keeps the texture value"
    );
    assert!(
        freed.0.lock().is_empty(),
        "the texture's GPU copy is not freed on a failed kind switch"
    );
    assert_eq!(
        server.generic_kind_of(id),
        Some("texture"),
        "the id keeps the texture kind"
    );
    assert_eq!(server.state(id), LoadState::Loaded);
    drop(texture);
    let _ = std::fs::remove_dir_all(&root);
}

/// Records the texture GPU copies the server frees.
#[derive(Default)]
struct Freed(Mutex<Vec<AssetId>>);

struct TextureUpload(Arc<Freed>);

impl PrepareAsset for TextureUpload {
    type Asset = CookedTexture;

    fn prepare(&self, _id: AssetId, asset: &CookedTexture) -> Result<u64, String> {
        Ok(u64::from(asset.width) * u64::from(asset.height))
    }

    fn unload(&self, id: AssetId) {
        self.0.0.lock().push(id);
    }
}

/// Item 4: a counted texture whose cooked file became an RGBA8 image takes the image kind on hot
/// reload (the reload reports it, the value moves to the image slab, the texture's GPU copy is
/// freed, and the same count now answers `load::<CookedImage>`), and back again.
#[test]
fn a_counted_texture_that_became_an_image_takes_the_image_kind_on_reload() {
    let root = regia_test_utils::unique_temp_dir("counted-kind-change");
    let cache = cooked_cache_dir(&root, "linux");
    std::fs::create_dir_all(&cache).expect("cache");
    let id = AssetId::new_random();
    let path = cache.join(cooked_file_name(id));
    write_atomic(&path, &texture_file(id));
    let mut server = open(&root, None);
    let freed = Arc::new(Freed::default());
    server.register_prepare(TextureUpload(Arc::clone(&freed)));

    let texture = server.load::<CookedTexture>(id).expect("load");
    server.wait_for_loads();
    assert!(server.get(&texture).is_some());

    // KTX2 -> PNG (an RGBA8 image).
    write_atomic(&path, &image_file(id));
    assert_eq!(
        server.reload_id(id).expect("reload"),
        Some(LoadedKind::Image),
        "the reload reports the new kind"
    );
    assert!(server.get(&texture).is_none(), "the texture value is gone");
    assert!(server.textures().handle_for_id(id).is_none());
    assert_eq!(*freed.0.lock(), vec![id], "the texture's GPU copy is freed");
    let image = server
        .load::<CookedImage>(id)
        .expect("the same id now loads as an image");
    assert_eq!(server.get(&image).map(|i| i.rgba8.len()), Some(16));
    assert_eq!(server.strong_count(id), 2, "both handles count the one asset");
    assert_eq!(
        server.generic_kind_of(id),
        Some("image"),
        "assets.handles lists it as an image"
    );
    assert_eq!(server.state(id), LoadState::Loaded);

    // And back to KTX2.
    drop(texture);
    write_atomic(&path, &texture_file(id));
    assert_eq!(server.reload_id(id).expect("reload"), Some(LoadedKind::Texture));
    assert!(server.get(&image).is_none(), "the image value is gone");
    assert!(server.images().handle_for_id(id).is_none());
    let texture = server.load::<CookedTexture>(id).expect("a texture again");
    assert!(server.get(&texture).is_some());
    assert_eq!(server.strong_count(id), 2);

    // A file that decodes as neither keeps the current value.
    write_atomic(&path, b"not a cooked file");
    assert!(server.reload_id(id).is_err());
    assert!(
        server.get(&texture).is_some(),
        "a failed reload keeps the texture"
    );
    drop((texture, image));
    let _ = std::fs::remove_dir_all(&root);
}
