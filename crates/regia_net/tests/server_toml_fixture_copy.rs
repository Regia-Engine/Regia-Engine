// SPDX-License-Identifier: MPL-2.0
//! `formats.rs` embeds crate-local copies of the frozen `server-toml` v4 corpus files
//! (`tests/fixtures/formats/server-toml/`) so the library's sources reach nothing outside the crate
//! (`cargo xtask check-packaging`, ADR-1485). The copies must stay byte-identical to the corpus files
//! in `regia_agent/tests/fixtures/format-corpus/`, which are the frozen ones.

use std::path::PathBuf;

#[test]
fn the_server_toml_v4_copies_match_the_frozen_corpus_files() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for name in ["v4.ron", "v4-playlist.ron"] {
        let copy = std::fs::read(manifest.join("tests/fixtures/formats/server-toml").join(name))
            .unwrap_or_else(|e| panic!("crate-local server-toml {name} copy: {e}"));
        let corpus = std::fs::read(
            manifest
                .join("../regia_agent/tests/fixtures/format-corpus/server-toml")
                .join(name),
        )
        .unwrap_or_else(|e| panic!("frozen server-toml {name} corpus file: {e}"));
        assert_eq!(
            copy, corpus,
            "the crate-local server-toml {name} copy drifted from the frozen corpus file"
        );
    }
}
