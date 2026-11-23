// SPDX-License-Identifier: MIT-0
//! `chase.rs` compiles in its own copies of the lookdev scatter sets (a sample reads no file outside
//! its folder at compile time, ADR-1485). The copies stay byte-identical to the originals.

use std::path::Path;

#[test]
fn scatter_sets_match_the_lookdev_originals() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    for name in ["mars.scatter.ron", "europa.scatter.ron"] {
        let mine = std::fs::read_to_string(manifest.join("scatter").join(name)).expect("terrain_lab copy");
        let original = std::fs::read_to_string(manifest.join("../lookdev/scatter").join(name))
            .expect("lookdev original");
        assert_eq!(
            mine, original,
            "{name}: copy samples/lookdev/scatter/{name} over the terrain_lab one"
        );
    }
}
