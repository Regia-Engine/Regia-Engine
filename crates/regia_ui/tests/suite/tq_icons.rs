// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): icon sheet validation, name parsing, the atlas loader and the
//! variant-picking rules at their boundaries. CPU only; no renderer, no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_ui::icons::{
    ICON_IMAGE_BASE, IconAtlas, IconError, IconPage, IconSheet, IconSprite, IconVariant, MAX_PAGE_SIDE,
    parse_icon_name, pick_variant,
};

fn page(w: u32, h: u32) -> IconPage {
    IconPage {
        width: w,
        height: h,
        rgba8: vec![255; (w * h * 4) as usize],
    }
}

fn sprite(name: &str, page: u32, rect: [u32; 4]) -> IconSprite {
    IconSprite {
        name: name.to_owned(),
        page,
        rect,
    }
}

fn variant(size_px: u32) -> IconVariant {
    IconVariant {
        size_px,
        image: 1,
        uv: [0.0, 0.0, 1.0, 1.0],
        aspect: 1.0,
    }
}

#[test]
fn icon_names_split_on_the_last_at_sign() {
    for (text, want) in [
        ("gear", Some(("gear", None))),
        ("gear@16", Some(("gear", Some(16)))),
        ("a@b@3", Some(("a@b", Some(3)))),
        ("gear@4294967295", Some(("gear", Some(u32::MAX)))),
        ("", None),
        ("@16", None),
        ("gear@", None),
        ("gear@0", None),
        ("gear@-4", None),
        ("gear@1.5", None),
        ("gear@x", None),
        ("gear@4294967296", None),
        ("gear@16 ", None),
    ] {
        assert_eq!(parse_icon_name(text), want, "{text:?}");
    }
}

#[test]
fn error_codes_are_protocol_safe() {
    let errors = [
        IconError::BadPage {
            page: 0,
            width: 0,
            height: 0,
            bytes: 0,
        },
        IconError::BadRect {
            name: String::new(),
            page: 0,
            rect: [0; 4],
        },
        IconError::BadName(String::new()),
        IconError::Duplicate {
            name: String::new(),
            size: 1,
        },
        IconError::BadGrid {
            cell: [0; 2],
            width: 0,
            height: 0,
        },
    ];
    let mut codes: Vec<_> = errors.iter().map(IconError::code).collect();
    for code in &codes {
        assert!(
            code.starts_with("icon_") && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{code}"
        );
    }
    codes.sort_unstable();
    codes.dedup();
    assert_eq!(codes.len(), errors.len(), "codes are distinct");
}

#[test]
fn pages_must_match_their_size_and_stay_under_the_limit() {
    let sheet = |p: IconPage| IconSheet {
        pages: vec![p],
        sprites: vec![],
    };
    assert!(sheet(page(4, 4)).validate().is_ok());
    let mut short = page(4, 4);
    short.rgba8.pop();
    assert_eq!(sheet(short).validate().unwrap_err().code(), "icon_bad_page");
    let mut long = page(4, 4);
    long.rgba8.push(0);
    assert_eq!(sheet(long).validate().unwrap_err().code(), "icon_bad_page");
    for (w, h) in [(0, 4), (4, 0), (MAX_PAGE_SIDE + 1, 1), (1, MAX_PAGE_SIDE + 1)] {
        let p = IconPage {
            width: w,
            height: h,
            rgba8: vec![],
        };
        assert_eq!(
            sheet(p).validate().unwrap_err().code(),
            "icon_bad_page",
            "{w}x{h}"
        );
    }
    // The error names the page that is wrong.
    let two = IconSheet {
        pages: vec![
            page(2, 2),
            IconPage {
                width: 2,
                height: 2,
                rgba8: vec![0; 3],
            },
        ],
        sprites: vec![],
    };
    assert!(matches!(two.validate(), Err(IconError::BadPage { page: 1, .. })));
}

#[test]
fn sprite_rectangles_must_fit_without_overflowing() {
    let with = |s: IconSprite| IconSheet {
        pages: vec![page(8, 8)],
        sprites: vec![s],
    };
    assert!(
        with(sprite("a", 0, [0, 0, 8, 8])).validate().is_ok(),
        "the whole page"
    );
    assert!(
        with(sprite("a", 0, [7, 7, 1, 1])).validate().is_ok(),
        "the last pixel"
    );
    for (page_index, rect) in [
        (0, [0, 0, 0, 4]),
        (0, [0, 0, 4, 0]),
        (0, [5, 0, 4, 4]),
        (0, [0, 5, 4, 4]),
        (0, [u32::MAX, 0, 2, 2]),
        (0, [1, 1, u32::MAX, 1]),
        (1, [0, 0, 1, 1]),
        (u32::MAX, [0, 0, 1, 1]),
    ] {
        let err = with(sprite("a", page_index, rect)).validate().unwrap_err();
        assert_eq!(err.code(), "icon_bad_rect", "{page_index} {rect:?}");
    }
    for name in ["", "@8", "a@", "a@0"] {
        assert_eq!(
            with(sprite(name, 0, [0, 0, 4, 4])).validate().unwrap_err().code(),
            "icon_bad_name",
            "{name:?}"
        );
    }
}

#[test]
fn two_drawings_of_one_icon_at_one_size_are_refused_even_via_the_source_height() {
    let sheet = |a: IconSprite, b: IconSprite| IconSheet {
        pages: vec![page(32, 32)],
        sprites: vec![a, b],
    };
    // `gear` (no size) is drawn at its height, 16: the same size as `gear@16`.
    let err = sheet(
        sprite("gear", 0, [0, 0, 16, 16]),
        sprite("gear@16", 0, [16, 0, 8, 8]),
    )
    .validate()
    .unwrap_err();
    assert!(matches!(err, IconError::Duplicate { size: 16, .. }), "{err:?}");
    assert!(
        sheet(
            sprite("gear@16", 0, [0, 0, 16, 16]),
            sprite("gear@24", 0, [16, 0, 16, 16])
        )
        .validate()
        .is_ok()
    );
    assert!(
        sheet(
            sprite("gear@16", 0, [0, 0, 16, 16]),
            sprite("cog@16", 0, [16, 0, 16, 16])
        )
        .validate()
        .is_ok()
    );
}

#[test]
fn grids_reject_cells_that_do_not_fit_and_cut_what_does() {
    for cell in [[0, 4], [4, 0], [9, 4], [4, 9]] {
        let err = IconSheet::from_grid(page(8, 8), cell, &["a"]).unwrap_err();
        assert_eq!(err.code(), "icon_bad_grid", "{cell:?}");
    }
    // 8x8 page, 4x4 cells: four cells. Extra names are dropped, empty names skip a cell.
    let sheet = IconSheet::from_grid(page(8, 8), [4, 4], &["a", "", "c", "d", "e", "f"]).unwrap();
    let cut: Vec<(&str, [u32; 4])> = sheet.sprites.iter().map(|s| (s.name.as_str(), s.rect)).collect();
    assert_eq!(
        cut,
        [("a", [0, 0, 4, 4]), ("c", [0, 4, 4, 4]), ("d", [4, 4, 4, 4])]
    );
    // A page that is not a multiple of the cell: the partial column is unused.
    let sheet = IconSheet::from_grid(page(10, 4), [4, 4], &["a", "b", "c"]).unwrap();
    assert_eq!(sheet.sprites.len(), 2);
    // No names, no sprites, still a valid sheet.
    assert!(
        IconSheet::from_grid(page(4, 4), [4, 4], &[])
            .unwrap()
            .sprites
            .is_empty()
    );
    // A name with a bad size is caught by validation.
    assert_eq!(
        IconSheet::from_grid(page(4, 4), [4, 4], &["a@0"])
            .unwrap_err()
            .code(),
        "icon_bad_name"
    );
}

#[test]
fn the_atlas_assigns_page_ids_in_order_and_later_sheets_override_sizes() {
    let mut atlas = IconAtlas::default();
    let first = IconSheet::from_grid(page(16, 16), [8, 8], &["gear@8", "star@8"]).unwrap();
    assert_eq!(atlas.add_sheet(first).unwrap(), 2);
    let second = IconSheet {
        pages: vec![page(32, 32), page(16, 16)],
        sprites: vec![
            sprite("gear@8", 0, [0, 0, 8, 8]),
            sprite("gear@16", 1, [0, 0, 16, 16]),
        ],
    };
    assert_eq!(atlas.add_sheet(second).unwrap(), 2);
    assert_eq!(atlas.sheet_count(), 2);
    let gear = atlas.variants("gear");
    assert_eq!(
        gear.iter().map(|v| v.size_px).collect::<Vec<_>>(),
        [8, 16],
        "smallest first, no duplicate size"
    );
    assert_eq!(
        gear[0].image,
        ICON_IMAGE_BASE + 1,
        "the 8 px drawing now comes from the second sheet's first page"
    );
    assert_eq!(gear[1].image, ICON_IMAGE_BASE + 2);
    assert_eq!(atlas.names(), ["gear", "star"]);
    assert!(atlas.contains("star") && !atlas.contains("moon"));
    assert!(atlas.variants("moon").is_empty());
    assert_eq!(
        atlas
            .pending_pages()
            .iter()
            .map(|p| (p.1, p.2))
            .collect::<Vec<_>>(),
        [(16, 16), (32, 32), (16, 16)]
    );
    // A refused sheet changes nothing.
    let generation = atlas.generation;
    let bad = IconSheet {
        pages: vec![page(2, 2)],
        sprites: vec![sprite("x", 0, [0, 0, 3, 3])],
    };
    assert!(atlas.add_sheet(bad).is_err());
    assert_eq!(
        (atlas.generation, atlas.sheet_count(), atlas.pending_pages().len()),
        (generation, 2, 3)
    );
    // UV rectangles are fractions of the page.
    assert_eq!(atlas.variants("gear")[1].uv, [0.0, 0.0, 1.0, 1.0]);
    atlas.clear();
    assert!(atlas.names().is_empty() && atlas.pending_pages().is_empty());
    assert!(atlas.generation > generation);
}

#[test]
fn variant_picking_follows_the_three_rules() {
    let variants = [variant(16), variant(24), variant(32)];
    // 1. A variant within a pixel of the target draws at its own size.
    for (target, size) in [(16.0, 16), (16.9, 16), (15.1, 16), (24.0, 24), (31.5, 32)] {
        let pick = pick_variant(&variants, target).unwrap();
        assert_eq!(
            (pick.variant.size_px, pick.draw_px, pick.upscaled),
            (size, size as f32, false),
            "{target}"
        );
    }
    // 2. Otherwise the smallest larger variant, scaled to the rounded target.
    let pick = pick_variant(&variants, 20.0).unwrap();
    assert_eq!(
        (pick.variant.size_px, pick.draw_px, pick.upscaled),
        (24, 20.0, false)
    );
    let pick = pick_variant(&variants, 0.4).unwrap();
    assert_eq!(
        (pick.variant.size_px, pick.draw_px),
        (16, 1.0),
        "never below one pixel"
    );
    // 3. Larger than every variant: the largest, scaled up and flagged.
    let pick = pick_variant(&variants, 100.0).unwrap();
    assert_eq!(
        (pick.variant.size_px, pick.draw_px, pick.upscaled),
        (32, 100.0, true)
    );
    // Nothing to pick, or nothing to pick for.
    assert_eq!(pick_variant(&[], 16.0), None);
    for bad in [0.0, -4.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(pick_variant(&variants, bad), None, "{bad}");
    }
    // Two variants within a pixel: the nearer one.
    let close = [variant(16), variant(17)];
    assert_eq!(pick_variant(&close, 16.8).unwrap().variant.size_px, 17);
    assert_eq!(pick_variant(&close, 16.2).unwrap().variant.size_px, 16);
}
