// SPDX-License-Identifier: MPL-2.0
//! The crate's copy of the det-navmesh book page (its doctest source, ADR-1485) matches the book.

/// `src/navmesh.rs` runs the page's example as a doctest from the crate's own copy, so a packaged
/// or vendored crate still builds; this keeps the copy equal to `book/agents/det-navmesh.md`.
#[test]
fn the_crate_copy_of_the_det_navmesh_page_matches_the_book() {
    let book = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../book/agents/det-navmesh.md");
    let text = std::fs::read_to_string(&book).expect("book/agents/det-navmesh.md");
    assert_eq!(
        text,
        include_str!("../fixtures/det-navmesh.md"),
        "edit book/agents/det-navmesh.md and copy it to crates/regia_nav_det/fixtures/det-navmesh.md"
    );
}
