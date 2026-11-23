// SPDX-License-Identifier: MPL-2.0
//! The v0.5 security review of mods (ADR-5870 § 4): one file of a mod is bounded on its own, not
//! only by the whole mod's 8 GiB budget, because a read and the mount's hash hold a file whole.

use std::path::Path;

use regia_asset::mods_vfs::{ModSource, ModsErrorCode, ModsLimits, ModsVfs};
use regia_asset::{
    AssetId, FPAK_BLOB_ALIGNMENT, FPAK_VERSION, ManifestEntry, PakEntry, PakToc, encode_pak_header,
};
use regia_test_utils::unique_temp_dir;

/// A pak whose one entry claims to decompress to `size` bytes over `stored` garbage bytes, so any
/// attempt to decompress it fails with a pak error and a refusal from the table of contents is
/// told apart (the helper of `threat_model_caps.rs`).
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

/// The mount bounded a pak entry by the whole mod's budget (8 GiB by default), so one Workshop
/// item of 128 MiB of 64:1 data made every launch allocate and hash 8 GiB on the main thread.
/// An entry over the per-file limit (1 GiB by default) is refused from the table of contents.
#[test]
fn a_pak_entry_over_the_file_limit_is_refused_from_its_table_of_contents() {
    let dir = unique_temp_dir("sec-pak-file-limit");
    let path = dir.join("big.fpak");
    let default_file = ModsLimits::default().max_file_bytes;
    assert_eq!(default_file, 1 << 30);
    pak_claiming(&path, default_file + 1, &[0xAB; 64]);
    assert_eq!(
        ModsVfs::new()
            .mount("m", ModSource::Pak(path.clone()))
            .map_err(|e| e.code),
        Err(ModsErrorCode::TooLarge),
    );
    // A lower limit holds the same way; at the limit the mount goes on to decompress and the
    // garbage is the pak error.
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
    assert_eq!(
        ModsVfs::with_limits(limits(4096))
            .mount("m", ModSource::Pak(path))
            .map_err(|e| e.code),
        Err(ModsErrorCode::Pak),
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A folder's file over the limit is refused too, and `mod_file_size` reports the mount-time size
/// a caller checks before it reads.
#[test]
fn a_folder_file_over_the_file_limit_is_refused() {
    let root = unique_temp_dir("sec-dir-file-limit");
    std::fs::write(root.join("a.bin"), [7_u8; 600]).expect("a");
    std::fs::write(root.join("b.bin"), [7_u8; 300]).expect("b");
    let limits = |max_file_bytes| ModsLimits {
        max_file_bytes,
        ..ModsLimits::default()
    };
    assert_eq!(
        ModsVfs::with_limits(limits(599))
            .mount("m", ModSource::Dir(root.clone()))
            .map_err(|e| e.code),
        Err(ModsErrorCode::TooLarge),
    );
    let mut vfs = ModsVfs::with_limits(limits(600));
    vfs.mount("m", ModSource::Dir(root.clone()))
        .expect("mounts at the limit");
    assert_eq!(vfs.mod_file_size("m", "a.bin").map_err(|e| e.code), Ok(600));
    assert_eq!(vfs.mod_file_size("m", "b.bin").map_err(|e| e.code), Ok(300));
    assert_eq!(
        vfs.mod_file_size("m", "c.bin").map_err(|e| e.code),
        Err(ModsErrorCode::NotFound)
    );
    assert!(vfs.mod_file_size("other", "a.bin").is_err());
    let _ = std::fs::remove_dir_all(&root);
}
