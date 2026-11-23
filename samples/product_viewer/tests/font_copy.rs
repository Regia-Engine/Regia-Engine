// SPDX-License-Identifier: MIT-0
//! The panel font is a copy of `regia_ui`'s, kept in the sample so it builds from its own folder (ADR-1485).
//! The copy and its licence must stay byte-identical with the originals; refresh both together.

use std::path::Path;

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn the_panel_font_and_its_licence_match_regia_ui() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let origin = here.join("../../crates/regia_ui/assets/fonts");
    let copy = here.join("assets/fonts");
    for name in ["Inter-Regular.ttf", "LICENSE-Inter.txt"] {
        assert!(
            read(&copy.join(name)) == read(&origin.join(name)),
            "samples/product_viewer/assets/fonts/{name} differs from crates/regia_ui/assets/fonts/{name}; copy it again"
        );
    }
    assert!(
        product_viewer::panel::INTER_REGULAR == read(&origin.join("Inter-Regular.ttf")).as_slice(),
        "the compiled-in font is not regia_ui's Inter Regular"
    );
}
