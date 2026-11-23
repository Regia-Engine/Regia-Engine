// SPDX-License-Identifier: MPL-2.0
//! Spec 12 conformance checks for behaviour that had no direct test (lane spec-04-12-assets,
//! ADR-5611): the pak layout of § 5 as the shipping writer produces it.

use regia_asset::{
    AssetId, AssetManifest, COOKED_FORMAT_VERSION, CookedHeader, FPAK_BLOB_ALIGNMENT,
    FPAK_COMPRESS_BLOCK_SIZE, FPAK_MAGIC, ManifestEntry, PakReader, cooked_file_name, encode_cooked_file,
};
use regia_asset_pipeline::pak_writer::{PakWriteOptions, write_pak};
use regia_core::id::TypePathHash;

const TYPE_PATH: &str = "regia_asset::CookedImage";

/// A cooked file with a payload of `len` bytes: a repeating ramp (compresses) or a pseudo-random
/// run (does not).
fn cooked(id: AssetId, len: usize, compressible: bool) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ id.0.as_u128() as u64;
    let payload: Vec<u8> = (0..len)
        .map(|i| {
            if compressible {
                (i % 7) as u8
            } else {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            }
        })
        .collect();
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: TypePathHash::of(TYPE_PATH),
        asset_id: id,
        importer_version: 1_000_000,
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    encode_cooked_file(&header, &payload).expect("encode cooked")
}

/// Write `assets` as cooked files and pack them with `options`; returns the pak path.
fn pack(name: &str, assets: &[(AssetId, Vec<u8>)], options: &PakWriteOptions) -> std::path::PathBuf {
    let dir = regia_test_utils::unique_temp_dir(name);
    let cooked_dir = dir.join("cooked");
    std::fs::create_dir_all(&cooked_dir).expect("cooked dir");
    let mut entries = Vec::new();
    let mut manifest = Vec::new();
    for (i, (id, bytes)) in assets.iter().enumerate() {
        std::fs::write(cooked_dir.join(cooked_file_name(*id)), bytes).expect("write cooked");
        entries.push((*id, TypePathHash::of(TYPE_PATH)));
        manifest.push(ManifestEntry {
            path: format!("assets/tex_{i}.png"),
            id: *id,
            type_path: TYPE_PATH.to_string(),
        });
    }
    let out = dir.join("base.fpak");
    write_pak(
        &out,
        &cooked_dir,
        &entries,
        &AssetManifest { entries: manifest },
        options,
    )
    .expect("write pak");
    out
}

/// Spec 12 § 5: every asset blob starts on a 4096-byte boundary, compressed or stored, and a
/// compressed blob is cut into 256 KiB lz4 blocks; the file opens with `FPAK`.
#[test]
fn every_pak_blob_is_aligned_to_4096_bytes() {
    assert_eq!(FPAK_BLOB_ALIGNMENT, 4096);
    assert_eq!(FPAK_COMPRESS_BLOCK_SIZE, 256 * 1024);
    // Odd sizes, so a writer that packed blobs back to back would land off the boundary.
    let sizes = [1usize, 4095, 4097, 300_001, 3 * 256 * 1024 + 17];
    for compress in [false, true] {
        let assets: Vec<(AssetId, Vec<u8>)> = sizes
            .iter()
            .enumerate()
            .map(|(i, &len)| {
                let id = AssetId::new_random();
                (id, cooked(id, len, i % 2 == 0))
            })
            .collect();
        let options = PakWriteOptions {
            compress,
            ..PakWriteOptions::default()
        };
        let path = pack("spec12_pak_align", &assets, &options);
        let bytes = std::fs::read(&path).expect("read pak");
        assert_eq!(&bytes[..4], &FPAK_MAGIC);
        let reader = PakReader::open(&path).expect("open pak");
        for (id, cooked_bytes) in &assets {
            let entry = reader.entry(*id).expect("entry");
            assert!(
                entry.offset.is_multiple_of(FPAK_BLOB_ALIGNMENT),
                "compress={compress}: blob at {} is not 4096-aligned",
                entry.offset
            );
            assert_eq!(entry.size, cooked_bytes.len() as u64);
            if entry.compressed_size < entry.size {
                // A compressed entry stores a block table: one u32 per 256 KiB block.
                assert_ne!(entry.block_table_offset, 0);
            }
            let read = reader.read_decompressed(*id).expect("read");
            assert_eq!(&read, cooked_bytes);
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
