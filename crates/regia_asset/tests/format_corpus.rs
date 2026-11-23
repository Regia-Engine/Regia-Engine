// SPDX-License-Identifier: MPL-2.0
//! Cooked meshes against the old-bytes corpus (ADR-2743): `crates/regia_agent/tests/fixtures/format-corpus/cooked-mesh/`.
//! The registry's crate does not build multi-part cooked files (`COVERED_ELSEWHERE`), so the four checks run here.
//! The version is the payload's, behind the cooked header.

#[path = "../../regia_agent/tests/format_corpus_support/mod.rs"]
mod format_corpus_support;

use regia_asset::pak::{FPAK_VERSION, PakEntry, PakReader, PakToc, encode_pak_header, flags};
use regia_asset::{
    AssetId, AssetTypeTag, COOKED_MESH_FORMAT_VERSION, CookedMesh, decode_cooked_file, encode_cooked_file,
};

/// Decode the payload through the migrating reader and encode it again at the current version, header kept.
fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let (mut header, payload) = decode_cooked_file(bytes).map_err(|e| e.to_string())?;
    if header.type_tag != AssetTypeTag::mesh().0 {
        return Err("not a cooked mesh".into());
    }
    let mesh = CookedMesh::from_postcard(payload).map_err(|e| e.to_string())?;
    let out = mesh.to_postcard().map_err(|e| e.to_string())?;
    header.payload_len = out.len() as u64;
    encode_cooked_file(&header, &out).map_err(|e| e.to_string())
}

fn read(bytes: &[u8]) -> Option<u32> {
    regia_asset::cooked_mesh_file_version(bytes).ok().flatten()
}

/// Sets `format_version` in a current-layout payload (the migrated output) and wraps it again.
fn patch(bytes: &[u8], version: u32) -> Option<Vec<u8>> {
    let (mut header, payload) = decode_cooked_file(bytes).ok()?;
    let mut mesh = CookedMesh::from_postcard(payload).ok()?;
    mesh.format_version = version;
    let out = mesh.to_postcard().ok()?;
    header.payload_len = out.len() as u64;
    encode_cooked_file(&header, &out).ok()
}

#[test]
fn bless_missing_current_fixtures() {
    if format_corpus_support::blessing() {
        format_corpus_support::bless_migrated("cooked-mesh", COOKED_MESH_FORMAT_VERSION, "fckd", &load);
    }
}

#[test]
fn every_cooked_mesh_fixture_loads_round_trips_and_refuses_the_future() {
    let problems = format_corpus_support::check_format_with(
        "cooked-mesh",
        COOKED_MESH_FORMAT_VERSION,
        &load,
        &read,
        &patch,
    );
    assert!(problems.is_empty(), "{problems:#?}");
}

/// A small valid pak with a raw and an lz4 entry (the fixture of `tests/fuzz.rs`, built the same way).
fn pak_fixture() -> Vec<u8> {
    use regia_core::id::TypePathHash;
    let raw_id = AssetId::sub_asset(AssetId::NIL, "fuzz/raw");
    let lz4_id = AssetId::sub_asset(AssetId::NIL, "fuzz/lz4");
    let raw: Vec<u8> = (0u8..48).collect();
    let packed: Vec<u8> = (0..300u32).map(|i| (i * 7 % 13) as u8).collect();
    let mut data = vec![0u8; 60];
    let raw_offset = data.len() as u64;
    data.extend_from_slice(&raw);
    let lz4_offset = data.len() as u64;
    let block = lz4_flex::compress_prepend_size(&packed);
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&block);
    let toc = PakToc {
        entries: vec![
            PakEntry {
                id: raw_id,
                type_tag: TypePathHash(1),
                offset: raw_offset,
                size: raw.len() as u64,
                compressed_size: raw.len() as u64,
                block_table_offset: 0,
            },
            PakEntry {
                id: lz4_id,
                type_tag: TypePathHash(2),
                offset: lz4_offset,
                size: packed.len() as u64,
                compressed_size: (4 + block.len()) as u64,
                block_table_offset: lz4_offset,
            },
        ],
        manifest: vec![regia_asset::ManifestEntry {
            path: "fuzz/raw.bin".into(),
            id: raw_id,
            type_path: "fuzz::Raw".into(),
        }],
        dependencies: vec!["base.fpak".into()],
    };
    let toc_offset = data.len() as u64;
    let toc_bytes = postcard::to_allocvec(&toc).expect("encode the table of contents");
    data.extend_from_slice(&toc_bytes);
    let hash = *blake3::hash(&data[..toc_offset as usize]).as_bytes();
    let header = encode_pak_header(
        FPAK_VERSION,
        flags::COMPRESSED,
        toc_offset,
        toc_bytes.len() as u64,
        &hash,
    );
    data[..60].copy_from_slice(&header);
    data
}

/// Opens the pak and reads every entry. The container has no writer to re-encode with, so a pak that reads is its
/// own canonical form.
fn pak_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let reader = PakReader::from_bytes(bytes.to_vec(), "corpus.fpak", bytes.len() as u64)
        .map_err(|e| e.to_string())?;
    let ids: Vec<AssetId> = reader.toc.entries.iter().map(|e| e.id).collect();
    for id in ids {
        reader.read_decompressed(id).map_err(|e| e.to_string())?;
    }
    Ok(bytes.to_vec())
}

#[test]
fn bless_the_pak_fixture() {
    if format_corpus_support::blessing() {
        format_corpus_support::bless_current(
            "pak",
            FPAK_VERSION,
            "fpak",
            &pak_fixture(),
            "built like crates/regia_asset/tests/fuzz.rs pak_fixture: a raw and an lz4 entry, a manifest and a dependency",
        );
    }
}

#[test]
fn every_pak_fixture_loads_and_refuses_the_future() {
    // Pak reading validates the version word, the table and every entry's bytes, but a written pak is
    // rebuilt (compressed blocks, ordering) rather than re-encoded: there is no byte-preserving round trip,
    // so the honest checks are load + future-refusal (`check_load_only`).
    let problems = format_corpus_support::check_load_only("pak", FPAK_VERSION, &pak_load);
    assert!(problems.is_empty(), "{problems:#?}");
}
