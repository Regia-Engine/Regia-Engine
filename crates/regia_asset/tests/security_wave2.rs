// SPDX-License-Identifier: MPL-2.0
//! Threat-model findings 21 and 33 (ported from the threat-model review's reproducers, ADR-3206;
//! fixed under ADR-4922). Each finding test failed before its fix.

use regia_asset::content::{ChunkEntry, ContentManifest, Delivery};
use regia_asset::{HEIGHTFIELD_MAX_EDGE, Heightfield, HeightfieldLoadError, HeightfieldValidationError};

fn manifest_with_files(files: &[&str]) -> ContentManifest {
    let mut manifest = ContentManifest::new("build-1", "linux");
    for (i, file) in files.iter().enumerate() {
        manifest.chunks.push(ChunkEntry {
            name: format!("chunk{i}"),
            file: (*file).to_owned(),
            size: 1,
            hash: "00".repeat(32),
            delivery: Delivery::Install,
            requires: Vec::new(),
            overrides: false,
            mount_order: i as u32,
            assets: 0,
            blocks: Vec::new(),
        });
    }
    manifest
}

fn accepted(files: &[&str]) -> bool {
    let text = manifest_with_files(files).to_ron_string().expect("ron");
    ContentManifest::from_ron_str(&text).is_ok()
}

/// Finding 21: a chunk's `file` may be any bare name, so a fetched manifest can name the client's own
/// state file, or a library the host later loads, as the place a download lands.
#[test]
fn finding_21_a_chunk_file_must_be_a_pak() {
    for bad in ["content-state.ron", "x.dll", "y.chunk.ron"] {
        let manifest = manifest_with_files(&[bad]);
        let text = manifest.to_ron_string().expect("ron");
        assert!(ContentManifest::from_ron_str(&text).is_err(), "{bad}");
    }
}

/// Finding 21: two chunks whose files differ only in case are one file on Windows and share a
/// `.part`, which the uniqueness check is there to prevent.
#[test]
fn finding_21_two_chunks_cannot_share_a_file_by_case() {
    let manifest = manifest_with_files(&["A.fpak", "a.fpak"]);
    let text = manifest.to_ron_string().expect("ron");
    assert!(ContentManifest::from_ron_str(&text).is_err());
}

/// The chunk file rule as a table, run on every host: Windows and the default macOS volume fold
/// case, and Windows drops a trailing dot or space, so these name one file there even when they
/// are two on Linux.
#[test]
fn chunk_file_names_follow_the_windows_rule_on_every_host() {
    let single = [
        ("base.fpak", true),
        ("Base-1_x.fpak", true),
        (".fpak", false),
        ("base.FPAK", false),
        ("base.fpak.", false),
        ("base.fpak ", false),
        ("base.fpak.part", false),
        ("base.fpak:alt", false),
        ("nul.fpak", false),
        ("COM1.fpak", false),
        ("b\u{e4}se.fpak", false),
        ("a/b.fpak", false),
        ("..fpak", false),
        ("content-manifest.ron", false),
        ("chunk-map.bin", false),
    ];
    for (file, ok) in single {
        assert_eq!(accepted(&[file]), ok, "{file:?}");
    }
    let pairs = [
        (["a.fpak", "b.fpak"], true),
        (["a.fpak", "a.fpak"], false),
        (["A.fpak", "a.fpak"], false),
        (["dlc_One.fpak", "DLC_one.fpak"], false),
        (["a-1.fpak", "a_1.fpak"], true),
    ];
    for (files, ok) in pairs {
        assert_eq!(accepted(&files), ok, "{files:?}");
    }
}

/// Postcard bytes of a heightfield header (`width`, `height`, sample count as LEB128) and `samples`
/// raw sample bytes (one byte per sample below 128).
fn heightfield_bytes(width: u64, height: u64, count: u64, samples: usize) -> Vec<u8> {
    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }
    let mut out = Vec::new();
    varint(width, &mut out);
    varint(height, &mut out);
    varint(count, &mut out);
    out.extend(std::iter::repeat_n(7u8, samples));
    out
}

/// Finding 33: a side over the cap is refused, even with every sample present (meshing amplifies a
/// heightfield about 56 times; before the fix any `u32` side was decoded).
#[test]
fn finding_33_a_heightfield_side_over_the_cap_is_refused() {
    let edge = u64::from(HEIGHTFIELD_MAX_EDGE);
    let over = heightfield_bytes(edge + 1, 2, (edge + 1) * 2, ((edge + 1) * 2) as usize);
    match Heightfield::from_postcard(&over) {
        Err(HeightfieldLoadError::Validation(HeightfieldValidationError::TooLarge { w, h, .. })) => {
            assert_eq!((u64::from(w), h), (edge + 1, 2));
        }
        other => panic!(
            "not refused for its size: {:?}",
            other.map(|h| (h.width, h.height))
        ),
    }
    let at = heightfield_bytes(edge, 2, edge * 2, (edge * 2) as usize);
    let hf = Heightfield::from_postcard(&at).expect("a side at the cap loads");
    assert_eq!((hf.width, hf.height), (HEIGHTFIELD_MAX_EDGE, 2));
    // An in-memory heightfield may be larger (a stitched tiled terrain goes to 65 536 per side), so
    // `validate` does not cap it; meshing one does.
    let big = Heightfield {
        width: HEIGHTFIELD_MAX_EDGE + 1,
        height: 2,
        samples: vec![0; (edge as usize + 1) * 2],
    };
    assert!(big.validate().is_ok());
    assert!(matches!(
        regia_asset::mesh_from_heightfield(&big, 1.0, 1.0, 0.0),
        Err(regia_asset::MeshValidationError::HeightfieldTooLarge { .. })
    ));
}

/// Finding 33: the header is checked against the payload before anything is decoded: a header
/// that claims more samples than the bytes can hold (every sample takes at least one byte), or a
/// sample count that is not `width * height`, is refused as such.
#[test]
fn finding_33_a_heightfield_header_must_fit_its_payload() {
    // 4096 x 4096 claimed (inside the cap), 16 sample bytes present.
    let short = heightfield_bytes(4096, 4096, 4096 * 4096, 16);
    assert!(
        matches!(
            Heightfield::from_postcard(&short),
            Err(HeightfieldLoadError::Validation(
                HeightfieldValidationError::PayloadTooShort { .. }
            ))
        ),
        "{:?}",
        Heightfield::from_postcard(&short).err()
    );
    // A count that disagrees with the dimensions.
    let wrong = heightfield_bytes(4, 4, 15, 15);
    assert!(matches!(
        Heightfield::from_postcard(&wrong),
        Err(HeightfieldLoadError::Validation(
            HeightfieldValidationError::MismatchedSampleCount { .. }
        ))
    ));
    // A sample count far past `usize` on any host, and a truncated header.
    assert!(Heightfield::from_postcard(&heightfield_bytes(2, 2, u64::MAX, 0)).is_err());
    assert!(Heightfield::from_postcard(&[0x80]).is_err());
    assert!(Heightfield::from_postcard(&[]).is_err());
    // A well-formed one still round-trips.
    let hf = Heightfield {
        width: 3,
        height: 2,
        samples: vec![0, 1, 65535, 300, 7, 9],
    };
    let bytes = hf.to_postcard().expect("encodes");
    assert_eq!(Heightfield::from_postcard(&bytes).expect("decodes"), hf);
}
