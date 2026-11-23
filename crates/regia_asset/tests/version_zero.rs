// SPDX-License-Identifier: MPL-2.0
//! audit-v034b-formats L5: these binary formats all start at version 1, yet their readers
//! refused only versions above the current one, so a file whose version word was zeroed (damage,
//! or a tool from before the format was versioned) decoded as a current file. The registry and
//! data-table readers already refused 0; these now do too.

use regia_asset::content::{ChunkMap, ChunkWhy, ContentError};
use regia_asset::{
    AssetId, COOKED_FORMAT_VERSION, CookedHeader, CookedIoError, decode_cooked_file, encode_cooked_file,
    pack_importer_version,
};
use regia_core::id::TypePathHash;

/// `bytes` with the little-endian `u32` version word at `at` set to 0.
fn zero_version(mut bytes: Vec<u8>, at: usize) -> Vec<u8> {
    bytes[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
    bytes
}

#[test]
fn a_cooked_file_header_of_version_zero_is_refused() {
    let header = |format_version| CookedHeader {
        format_version,
        type_tag: TypePathHash::of("regia_asset::CpuMesh"),
        asset_id: AssetId::new_random(),
        importer_version: pack_importer_version(1, 0, 0),
        dependencies: Vec::new(),
        payload_len: 3,
    };
    let good = encode_cooked_file(&header(COOKED_FORMAT_VERSION), b"abc").expect("encode");
    decode_cooked_file(&good).expect("version 1 reads");
    let zero = encode_cooked_file(&header(0), b"abc").expect("encode");
    assert!(matches!(
        decode_cooked_file(&zero),
        Err(CookedIoError::UnsupportedVersion { found: 0, .. })
    ));
}

#[test]
fn a_chunk_map_of_version_zero_is_refused() {
    let bytes = ChunkMap::default().to_bytes().expect("encode");
    ChunkMap::from_bytes(&bytes).expect("version 1 reads");
    assert!(matches!(
        ChunkMap::from_bytes(&zero_version(bytes, 8)),
        Err(ContentError::ManifestVersion { found: 0, .. })
    ));
}

#[test]
fn a_chunk_why_table_of_version_zero_is_refused() {
    let bytes = ChunkWhy::default().to_bytes().expect("encode");
    ChunkWhy::from_bytes(&bytes).expect("version 1 reads");
    assert!(matches!(
        ChunkWhy::from_bytes(&zero_version(bytes, 8)),
        Err(ContentError::ManifestVersion { found: 0, .. })
    ));
}
