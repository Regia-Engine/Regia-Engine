// SPDX-License-Identifier: MPL-2.0
//! Open findings of the threat-model review in code this lane does not own (ADR-3206). Each test
//! fails today and passes once the finding is fixed; the number names it in the lane report.

use regia_asset::content::{ChunkEntry, ContentManifest, Delivery};

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

/// Finding 21: a chunk's `file` may be any bare name, so a fetched manifest can name the client's own
/// state file, or a library the host later loads, as the place a download lands.
#[test]
#[ignore = "finding 21: open, routed by the lead (regia_asset::content::manifest)"]
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
#[ignore = "finding 21: open, routed by the lead (regia_asset::content::manifest)"]
fn finding_21_two_chunks_cannot_share_a_file_by_case() {
    let manifest = manifest_with_files(&["A.fpak", "a.fpak"]);
    let text = manifest.to_ron_string().expect("ron");
    assert!(ContentManifest::from_ron_str(&text).is_err());
}
