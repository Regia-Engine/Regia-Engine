// SPDX-License-Identifier: MPL-2.0
//! The size limits a hostile mod meets before it costs memory (ADR-3206, `docs/security/threat-model.md`).

use std::path::{Path, PathBuf};

use regia_asset::mods_vfs::{ModSource, ModsErrorCode, ModsLimits, ModsVfs};
use regia_asset::{
    AssetId, FPAK_BLOB_ALIGNMENT, FPAK_VERSION, ManifestEntry, PakEntry, PakToc, encode_pak_header,
};
use regia_test_utils::unique_temp_dir;

/// A pak whose one entry claims to be compressed and to decompress to `size` bytes over `stored`.
/// Its blocks are garbage, so anything that tries to decompress it fails with a pak error, not with
/// the limit: the test can tell the two apart.
fn pak_claiming(path: &Path, size: u64, stored: &[u8]) {
    let mut data = vec![0_u8; 60];
    let rem = data.len() % FPAK_BLOB_ALIGNMENT as usize;
    if rem != 0 {
        data.resize(data.len() + (FPAK_BLOB_ALIGNMENT as usize - rem), 0);
    }
    let id = AssetId(regia_core::id::Guid(1));
    let offset = data.len() as u64;
    data.extend_from_slice(stored);
    let toc = PakToc {
        entries: vec![PakEntry {
            id,
            type_tag: regia_core::id::TypePathHash::of("regia_asset::Blob"),
            offset,
            size,
            compressed_size: stored.len() as u64,
            block_table_offset: offset,
        }],
        manifest: vec![ManifestEntry {
            path: "big.bin".to_owned(),
            id,
            type_path: "regia_asset::Blob".into(),
        }],
        dependencies: Vec::new(),
    };
    let toc_offset = data.len() as u64;
    let toc_bytes = postcard::to_allocvec(&toc).expect("toc");
    data.extend_from_slice(&toc_bytes);
    let hash = *blake3::hash(&data[..toc_offset as usize]).as_bytes();
    let header = encode_pak_header(FPAK_VERSION, 0, toc_offset, toc_bytes.len() as u64, &hash);
    data[..60].copy_from_slice(&header);
    std::fs::write(path, data).expect("write pak");
}

/// Audit ADR-3206 finding 8: a Workshop pak of 64:1 data was decompressed whole at mount (to hash
/// it) before `max_mod_bytes` was looked at. The declared size is checked first.
#[test]
fn a_pak_over_the_mod_limit_is_refused_from_its_table_of_contents() {
    let dir = unique_temp_dir("threat-pak-limit");
    let path = dir.join("big.fpak");
    pak_claiming(&path, 4096, &[0xAB; 64]);
    let mut limited = ModsVfs::with_limits(ModsLimits {
        max_mod_bytes: 4095,
        ..ModsLimits::default()
    });
    assert_eq!(
        limited
            .mount("m", ModSource::Pak(path.clone()))
            .map_err(|e| e.code),
        Err(ModsErrorCode::TooLarge),
    );
    // At the limit the mount goes on to decompress, and the garbage is the pak error: the refusal
    // above came before any decompression.
    let mut at_limit = ModsVfs::with_limits(ModsLimits {
        max_mod_bytes: 4096,
        ..ModsLimits::default()
    });
    assert_eq!(
        at_limit.mount("m", ModSource::Pak(path)).map_err(|e| e.code),
        Err(ModsErrorCode::Pak),
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn tree(tag: &str, files: &[(&str, &[u8])]) -> PathBuf {
    let root = unique_temp_dir(tag);
    for (rel, bytes) in files {
        let file = root.join(rel);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
        std::fs::write(file, bytes).expect("file");
    }
    root
}

/// `limits_are_enforced` in `mods_vfs.rs` covers file and mod counts; these are the other two.
#[test]
fn a_folder_over_the_byte_limit_is_refused() {
    let root = tree("threat-bytes", &[("a", &[0_u8; 600]), ("b", &[0_u8; 600])]);
    let limits = |max_mod_bytes| ModsLimits {
        max_mod_bytes,
        ..ModsLimits::default()
    };
    assert_eq!(
        ModsVfs::with_limits(limits(1199))
            .mount("m", ModSource::Dir(root.clone()))
            .map_err(|e| e.code),
        Err(ModsErrorCode::TooLarge),
    );
    assert!(
        ModsVfs::with_limits(limits(1200))
            .mount("m", ModSource::Dir(root.clone()))
            .is_ok()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_folder_nested_past_the_depth_limit_is_refused() {
    let root = tree("threat-depth", &[("a/b/c/d/f", b"x")]);
    let limits = |max_depth| ModsLimits {
        max_depth,
        ..ModsLimits::default()
    };
    assert_eq!(
        ModsVfs::with_limits(limits(3))
            .mount("m", ModSource::Dir(root.clone()))
            .map_err(|e| e.code),
        Err(ModsErrorCode::TooLarge),
    );
    assert!(
        ModsVfs::with_limits(limits(4))
            .mount("m", ModSource::Dir(root.clone()))
            .is_ok()
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `pak.rs` checks every entry's range when the pak opens; `entry_range` only checks it again on a
/// read. A table of contents that points past the blobs does not open at all.
#[test]
fn a_pak_entry_past_the_blob_area_does_not_open() {
    use regia_asset::{PakError, PakReader};
    let dir = unique_temp_dir("threat-pak-bounds");
    let path = dir.join("bad.fpak");
    pak_claiming(&path, 64, &[0xAB; 64]);
    // Move the entry far past the end by rewriting the stored table.
    let mut data = std::fs::read(&path).expect("read");
    let header = regia_asset::PakHeader::from_bytes(&data).expect("header");
    let toc_start = header.toc_offset as usize;
    let toc_end = toc_start + header.toc_len as usize;
    let mut toc: PakToc = postcard::from_bytes(&data[toc_start..toc_end]).expect("toc");
    toc.entries[0].offset = header.toc_offset + 1;
    let bytes = postcard::to_allocvec(&toc).expect("toc");
    data.truncate(toc_start);
    data.extend_from_slice(&bytes);
    let hash = *blake3::hash(&data[..toc_start]).as_bytes();
    let new_header = encode_pak_header(FPAK_VERSION, 0, header.toc_offset, bytes.len() as u64, &hash);
    data[..60].copy_from_slice(&new_header);
    std::fs::write(&path, data).expect("write");
    assert!(matches!(
        PakReader::open(&path),
        Err(PakError::OutOfBounds { .. })
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The content client reads a manifest of at most 16 MiB, whatever the server says.
#[test]
fn a_manifest_over_the_limit_is_refused() {
    let dir = unique_temp_dir("threat-manifest-cap");
    std::fs::write(dir.join("latest.ron"), "\"build-2\"").expect("latest");
    std::fs::create_dir_all(dir.join("build-2")).expect("build dir");
    let big = std::fs::File::create(dir.join("build-2/content-manifest.ron")).expect("manifest");
    big.set_len((16 << 20) + 1).expect("sparse file");
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let stats = std::sync::Arc::new(regia_asset::content::ServeStats::default());
    let root = dir.clone();
    std::thread::spawn(move || regia_asset::content::serve_dir(listener, root, stats));
    let running = regia_asset::content::ContentManifest::new("build-1", "linux");
    let error = regia_asset::content::fetch_manifest(&url, &running)
        .expect_err("too large")
        .to_string();
    assert!(error.contains("larger than"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `content serve` answers 404 for a folder under its root: it serves files, not listings.
#[test]
fn the_content_server_does_not_serve_a_folder() {
    use std::io::{Read, Write};
    let dir = unique_temp_dir("threat-serve-dir");
    std::fs::create_dir_all(dir.join("sub")).expect("sub");
    std::fs::write(dir.join("sub/a.fpak"), b"a").expect("file");
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stats = std::sync::Arc::new(regia_asset::content::ServeStats::default());
    let root = dir.clone();
    std::thread::spawn(move || regia_asset::content::serve_dir(listener, root, stats));
    let status = |path: &str| {
        let mut stream = std::net::TcpStream::connect(addr).expect("connect");
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())
            .expect("send");
        let mut text = String::new();
        stream.read_to_string(&mut text).expect("answer");
        text.split_whitespace().nth(1).unwrap_or_default().to_string()
    };
    assert_eq!(status("/sub/a.fpak"), "200");
    assert_eq!(status("/sub"), "404");
    assert_eq!(status("/sub/"), "404");
    let _ = std::fs::remove_dir_all(&dir);
}

/// sec-review-assets-agent (ADR-5869) finding 1: the reader of a mod pak took `max_mod_bytes` (8 GiB
/// by default) as its per-entry cap, so one 64:1 entry of a 128 MiB Workshop pak was an 8 GiB
/// allocation at mount. One entry is now held to `max_file_bytes` (ADR-5993 folded the separate
/// `max_pak_entry_bytes` into it), from its table of contents, before anything is decompressed.
#[test]
fn a_pak_entry_over_the_entry_limit_is_refused_from_its_table_of_contents() {
    let dir = unique_temp_dir("threat-pak-entry-limit");
    let path = dir.join("big.fpak");
    pak_claiming(&path, 4096, &[0xAB; 64]);
    let limits = |max_file_bytes| ModsLimits {
        max_file_bytes,
        ..ModsLimits::default()
    };
    assert_eq!(
        ModsVfs::with_limits(limits(4095))
            .mount("m", ModSource::Pak(path.clone()))
            .map_err(|e| e.code),
        Err(ModsErrorCode::TooLarge),
    );
    // At the limit the mount goes on to decompress the garbage: the refusal above came first.
    assert_eq!(
        ModsVfs::with_limits(limits(4096))
            .mount("m", ModSource::Pak(path))
            .map_err(|e| e.code),
        Err(ModsErrorCode::Pak),
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The default per-entry cap of a mod pak is far below the per-mod cap: one entry cannot ask for
/// the whole mod budget in one allocation.
#[test]
fn the_default_pak_entry_limit_is_one_gib() {
    let limits = ModsLimits::default();
    assert_eq!(limits.max_file_bytes, 1024 * 1024 * 1024);
    assert!(limits.max_file_bytes < limits.max_mod_bytes);
}
