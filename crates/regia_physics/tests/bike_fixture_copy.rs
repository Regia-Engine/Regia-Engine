// SPDX-License-Identifier: MPL-2.0
//! `vehicle_bike::asset`'s tests embed a crate-local copy of the frozen `bike` v1 corpus file
//! (`tests/fixtures/formats/bike/v1.ron`) so the crate's sources reach nothing outside it
//! (`cargo xtask check-packaging`, ADR-1485). The copy must stay byte-identical to the corpus file in
//! `regia_agent/tests/fixtures/format-corpus/`, which is the frozen one.

use std::path::PathBuf;

#[test]
fn the_bike_v1_copy_matches_the_frozen_corpus_file() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let copy =
        std::fs::read(manifest.join("tests/fixtures/formats/bike/v1.ron")).expect("crate-local bike v1 copy");
    let corpus = std::fs::read(manifest.join("../regia_agent/tests/fixtures/format-corpus/bike/v1.ron"))
        .expect("frozen bike v1 corpus file");
    assert_eq!(
        copy, corpus,
        "the crate-local bike v1 copy drifted from the frozen corpus file"
    );
}
