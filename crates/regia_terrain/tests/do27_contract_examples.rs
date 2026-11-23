// SPDX-License-Identifier: MPL-2.0
//! The worked examples of the DO27 interface contract (`book/user-guide/do27-interface-contract.md`,
//! `docs/specs/25-sim-bridge-and-do27.md` § 7) are real: the `<map>.holes.ron` example parses into a
//! valid [`HolesMask`] that cuts where the page says, and the object-key formula keeps clear of the
//! renderer's reserved key ranges. No GPU, no DO27 maps.
//!
//! The page is read from the repository, so a page edit that breaks an example fails here. The
//! example is parsed by [`HolesMask::from_holes_ron`], the loader the shell uses, so the `version`
//! field and the validation rules are checked too.

use std::path::PathBuf;

use regia_render::{EPHEMERAL_KEY, SHADOW_LOD_MESH_TAG};
use regia_terrain::{HOLES_FORMAT_VERSION, HolesFileError, HolesMask, MAX_HOLE_WINDOWS};

const PAGE: &str = "book/user-guide/do27-interface-contract.md";
const SPEC: &str = "docs/specs/25-sim-bridge-and-do27.md";

fn repo_file(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The bodies of the fences whose info string is exactly `info`, in order.
fn fences(text: &str, info: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut open: Option<String> = None;
    for line in text.lines() {
        let fence = line.trim_start().strip_prefix("```");
        if let Some(buf) = open.as_mut() {
            if fence.is_some() {
                out.extend(open.take());
            } else {
                buf.push_str(line);
                buf.push('\n');
            }
        } else if fence.is_some_and(|rest| rest.trim() == info) {
            open = Some(String::new());
        }
    }
    out
}

/// The page's `<map>.holes.ron` example.
fn holes_example() -> String {
    let mut blocks = fences(&repo_file(PAGE), "ron,do27-holes");
    assert_eq!(blocks.len(), 1, "the page carries exactly one holes example");
    blocks.remove(0)
}

#[test]
fn the_holes_example_is_a_valid_mask() {
    let mask = HolesMask::from_holes_ron(&holes_example()).expect("the example loads as a holes file");
    mask.validate().expect("the example is a valid mask");
    // polygon, rect, ellipse and raster: the four shape kinds the page lists.
    assert_eq!(mask.shapes.len(), 4);
    assert!(mask.holes.is_empty());
    assert!(mask.holes.len() + mask.shapes.len() <= MAX_HOLE_WINDOWS);
    assert_eq!(MAX_HOLE_WINDOWS, 64, "the contract says at most 64 shapes");
}

#[test]
fn the_holes_example_cuts_where_the_page_says() {
    let mask = HolesMask::from_holes_ron(&holes_example()).expect("loads");
    // The 30 m mouth polygon around the origin, the rotated ramp trench, the collapse pit.
    assert!(mask.contains(0.0, 0.0), "inside the mouth polygon");
    assert!(mask.contains(40.0, 2.0), "inside the trench");
    assert!(mask.contains(-40.0, 0.0), "inside the pit");
    // The raster: the four centre texels are 255, the outer ring 0.
    assert!(mask.contains(-60.0, 30.0), "inside the raster's covered core");
    assert!(
        !mask.contains(-67.0, 30.0),
        "the raster's uncovered edge is ground"
    );
    // Open ground between and beyond.
    assert!(!mask.contains(20.0, 40.0));
    assert!(!mask.contains(200.0, 200.0));
}

#[test]
fn a_newer_holes_version_is_refused() {
    // Version 2 (named holes, ADR-1595); the page's example is a version 1 file, which still loads.
    assert_eq!(HOLES_FORMAT_VERSION, 2, "the page documents version 2");
    assert!(
        repo_file(PAGE).contains("`HOLES_FORMAT_VERSION` is 2"),
        "the page names the version"
    );
    let newer = holes_example().replacen("version: 1", "version: 3", 1);
    assert!(matches!(
        HolesMask::from_holes_ron(&newer),
        Err(HolesFileError::NewerVersion(3))
    ));
}

/// The page's `<map>.viscells.ron` example (section 6).
fn viscells_example() -> String {
    let mut blocks = fences(&repo_file(PAGE), "ron,do27-viscells");
    assert_eq!(blocks.len(), 1, "the page carries exactly one viscells example");
    blocks.remove(0)
}

#[test]
fn the_viscells_example_loads_and_locates_where_the_page_says() {
    use regia_render::viscells::{VISCELLS_FORMAT_VERSION, VisCells};
    let cells = VisCells::from_ron(&viscells_example()).expect("the example loads as a viscells file");
    assert_eq!(VISCELLS_FORMAT_VERSION, 1, "the page documents version 1");
    // Cells 0 (outdoors), 1 (tube_a) and 2 (bunker, two boxes); the mouth and the bunker door.
    assert_eq!(cells.cells.len(), 3);
    assert_eq!(cells.cells[2].bounds.len(), 2);
    assert_eq!(cells.portals.len(), 2);
    assert_eq!(cells.locate([120.0, -18.0, 40.0].into()), 1, "inside tube_a");
    assert_eq!(
        cells.locate([150.0, -20.0, 158.0].into()),
        2,
        "inside the bunker's annex"
    );
    assert_eq!(
        cells.locate([0.0, 50.0, 0.0].into()),
        0,
        "on the surface: outdoors"
    );
    // The re-encoded file loads to the same set (what `regia upgrade-project` would write).
    let again = VisCells::from_ron(&cells.to_ron().expect("encodes")).expect("reloads");
    assert_eq!(again, cells);
}

#[test]
fn the_viscells_rules_on_the_page_are_the_loaders() {
    use regia_render::viscells::{VisCells, VisCellsError};
    let example = viscells_example();
    let newer = example.replacen("version: 1", "version: 2", 1);
    assert!(matches!(
        VisCells::from_ron(&newer),
        Err(VisCellsError::NewerVersion(2))
    ));
    let to_itself = example.replacen("(a: 1, b: 2,", "(a: 2, b: 2,", 1);
    assert_eq!(
        VisCells::from_ron(&to_itself).map(|_| ()).map_err(|e| e.code()),
        Err("viscells_self_portal")
    );
    let unknown = example.replacen("(a: 1, b: 2,", "(a: 1, b: 9,", 1);
    assert_eq!(
        VisCells::from_ron(&unknown).map(|_| ()).map_err(|e| e.code()),
        Err("viscells_unknown_cell")
    );
    let page = repo_file(PAGE);
    for code in [
        "viscells_unknown_cell",
        "viscells_self_portal",
        "viscells_portal_not_planar",
        "viscells_portal_not_convex",
    ] {
        assert!(page.contains(code), "the page names {code}");
    }
}

/// The formula printed on the page (section 8).
fn object_key(entity_id: u64, part_index: u8) -> u64 {
    debug_assert!(entity_id < (1 << 48));
    (entity_id << 8) | u64::from(part_index)
}

#[test]
fn the_page_prints_the_formula_the_test_checks() {
    // The test's copy of the formula must match the page's text, so a page edit that changes
    // the packing (shift or part width) fails here instead of leaving the copy green.
    let page = repo_file(PAGE);
    assert!(
        page.contains("(entity_id << 8) | u64::from(part_index)"),
        "the page's object-key formula changed; update `object_key` and re-check the ranges"
    );
    assert!(page.contains("debug_assert!(entity_id < (1 << 48))") || page.contains("1 << 48"));
}

#[test]
fn object_keys_stay_clear_of_the_reserved_ranges() {
    let ids = [1_u64, 2, 255, 256, 1 << 20, (1 << 48) - 2, (1 << 48) - 1];
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        for part in [0_u8, 1, 7, 254, 255] {
            let key = object_key(id, part);
            assert_ne!(key, 0, "key 0 is ephemeral");
            assert!(key < 1 << 56, "key {key:#x} is below 2^56");
            assert_ne!(
                key >> 56,
                EPHEMERAL_KEY >> 56,
                "top byte 0xFE is the ephemeral range"
            );
            assert_ne!(
                key >> 56,
                SHADOW_LOD_MESH_TAG >> 56,
                "top byte 0xFF is the shadow-LOD tag"
            );
            assert!(seen.insert(key), "keys are unique per (entity, part)");
        }
    }
    assert_eq!(EPHEMERAL_KEY >> 56, 0xFE);
    assert_eq!(SHADOW_LOD_MESH_TAG >> 56, 0xFF);
}

#[test]
fn the_spec_and_the_page_name_every_interface() {
    let spec = repo_file(SPEC);
    assert!(spec.contains("## 7. DO27 interface contracts (v0.2.0)"));
    let page = repo_file(PAGE);
    assert!(page.starts_with("# DO27 interface contracts (v0.2.0)"));
    for name in [
        "HolesMask",
        "MapLayer",
        "world_pick",
        "TerrainHeightEdits",
        "ExposureZone",
        "AudioReverbZone",
        "VisCell",
        "Steam::client",
        "object_key",
        "regia_voxel_core",
    ] {
        assert!(spec.contains(name), "spec 25 § 7 names {name}");
        assert!(page.contains(name), "the book page names {name}");
    }
    let summary = repo_file("book/SUMMARY.md");
    assert!(summary.contains("user-guide/do27-interface-contract.md"));
}

/// The page's `HeightEdit` example (section 3), as the shell would build it.
#[derive(serde::Deserialize)]
#[serde(rename = "HeightEdit")]
struct PageHeightEdit {
    col: u32,
    row: u32,
    width: u32,
    height: u32,
    heights_mm: Vec<i32>,
}

/// ADR-7585: the page's runtime height edit is a valid `HeightEdit` that lands on the map's
/// 16-bit grid within one step of its millimetres, and the page names the limits the code has.
#[test]
fn the_height_edit_example_lands_on_the_grid() {
    use regia_terrain::{
        HeightEdit, MAX_EDIT_SIDE, MAX_EDITS_PER_FRAME, PatchTarget, TerrainHeightEdits,
        TerrainHeightPatches, TerrainMap,
    };
    let mut blocks = fences(&repo_file(PAGE), "ron,do27-height-edit");
    assert_eq!(
        blocks.len(),
        1,
        "the page carries exactly one height-edit example"
    );
    let page: PageHeightEdit = ron::from_str(&blocks.remove(0)).expect("the example parses");
    let n = 513u32;
    let map = TerrainMap::from_heights(n, n, vec![12.0; (n * n) as usize], 2048.0, 64.0, -32.0).expect("map");
    let patches = TerrainHeightPatches::default();
    patches.set_target(PatchTarget::of(&map));
    let edits = TerrainHeightEdits::new(patches.clone());
    let edit = HeightEdit {
        col: page.col,
        row: page.row,
        width: page.width,
        height: page.height,
        heights_mm: page.heights_mm.clone(),
    };
    let id = edits.push(edit).unwrap_or_else(|e| panic!("{}: {e}", e.code()));
    let mut applied = map.clone();
    for band in patches.take_frame() {
        band.apply(&mut applied).expect("same grid");
    }
    assert_eq!(patches.state().completed, id);
    let step = map.height_scale() / 65535.0;
    for (i, mm) in page.heights_mm.iter().enumerate() {
        let (c, r) = (page.col + i as u32 % page.width, page.row + i as u32 / page.width);
        let h = applied.heights()[(r * n + c) as usize];
        assert!(
            (h - *mm as f32 * 1e-3).abs() <= step,
            "sample {i}: {h} m for {mm} mm"
        );
    }
    let page_text = repo_file(PAGE);
    assert_eq!((MAX_EDIT_SIDE, MAX_EDITS_PER_FRAME), (128, 16));
    for needle in [
        "over 128 samples",
        "edit_too_large",
        "edit_frame_full",
        "edit_out_of_range",
        "terrain_height_edits",
    ] {
        assert!(page_text.contains(needle), "the page names {needle}");
    }
    assert!(repo_file(SPEC).contains("### 7.4 `TerrainHeightEdits` (as built, stages 1 and 2"));
}
