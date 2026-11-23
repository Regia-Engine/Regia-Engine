// SPDX-License-Identifier: MPL-2.0
//! `formats.rs` embeds crate-local copies of the frozen `playlist-ron` corpus files
//! (`tests/fixtures/formats/playlist-ron/v1.ron` and `v2.ron`) so the library's sources reach nothing
//! outside the crate (`cargo xtask check-packaging`, ADR-1485). Each copy must stay byte-identical to
//! the corpus file in `regia_agent/tests/fixtures/format-corpus/`, which is the frozen one.

use std::path::PathBuf;

fn assert_copy_matches(file: &str) {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let copy = std::fs::read(manifest.join("tests/fixtures/formats/playlist-ron").join(file))
        .unwrap_or_else(|e| panic!("crate-local playlist-ron {file} copy: {e}"));
    let corpus = std::fs::read(
        manifest
            .join("../regia_agent/tests/fixtures/format-corpus/playlist-ron")
            .join(file),
    )
    .unwrap_or_else(|e| panic!("frozen playlist-ron {file} corpus file: {e}"));
    assert_eq!(
        copy, corpus,
        "the crate-local playlist-ron {file} copy drifted from the frozen corpus file"
    );
}

#[test]
fn the_playlist_v1_copy_matches_the_frozen_corpus_file() {
    assert_copy_matches("v1.ron");
}

/// The v2 copy (ADR-10064) that `formats.rs` reads in `playlist_ron_v2_fixture_loads`.
#[test]
fn the_playlist_v2_copy_matches_the_frozen_corpus_file() {
    assert_copy_matches("v2.ron");
}
