// SPDX-License-Identifier: MIT-0
//! `lib.rs` runs the capture-the-flag tutorial's Rust examples as doctests from the sample's own copy
//! of the book page (a sample reads no file outside its folder at compile time, ADR-1485,
//! `cargo xtask check-packaging`). The copy stays byte-identical to the book page.

use std::path::Path;

#[test]
fn the_tutorial_copy_matches_the_book_page() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mine = std::fs::read(manifest.join("doc/capture-the-flag.md")).expect("the arena copy");
    let book =
        std::fs::read(manifest.join("../../book/tutorials/capture-the-flag.md")).expect("the book page");
    assert!(
        mine == book,
        "copy book/tutorials/capture-the-flag.md over samples/arena/doc/capture-the-flag.md"
    );
}
