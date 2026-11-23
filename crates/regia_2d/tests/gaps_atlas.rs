// SPDX-License-Identifier: MPL-2.0
//! Atlas format and asset-store edge cases (test-gap audit): cooked validation at exact page and
//! trim boundaries, multi-page lookups, name ordering, source path rules, image limits, damaged
//! payloads, key uniqueness and the failure-retry bookkeeping. CPU only.

use regia_2d::atlas::{AtlasError, AtlasSprite, NineSlice, RgbaImage, SpriteAtlas};
use regia_2d::sprite::{LoadedAtlas, SpriteAssets};
use regia_2d::{AtlasSource, Sprite, SpriteSource, TileMap, View2d};

fn image(w: u32, h: u32) -> RgbaImage {
    RgbaImage {
        width: w,
        height: h,
        rgba8: vec![1; (w * h * 4) as usize],
    }
}

fn entry(name: &str, page: u32, rect: [u32; 4], source: [u32; 2], trim: [u32; 2]) -> AtlasSprite {
    AtlasSprite {
        name: name.into(),
        page,
        rect,
        source_size: source,
        trim_offset: trim,
        pivot: [0.5, 0.5],
        nine_slice: None,
    }
}

fn atlas_with(pages: Vec<RgbaImage>, sprites: Vec<AtlasSprite>) -> SpriteAtlas {
    let mut atlas = SpriteAtlas::single("seed", image(1, 1));
    atlas.pages = pages;
    atlas.sprites = sprites;
    atlas
}

/// Catches an off-by-one in the "inside its page" check: a sprite touching the last row and
/// column is legal, one more pixel is not, and a huge origin must not wrap.
#[test]
fn sprites_may_touch_the_page_edge_but_not_cross_it() {
    let page = || vec![image(16, 8)];
    assert!(
        atlas_with(page(), vec![entry("a", 0, [12, 4, 4, 4], [4, 4], [0, 0])])
            .validate()
            .is_ok()
    );
    for (rect, why) in [
        ([13, 4, 4, 4], "one pixel right"),
        ([12, 5, 4, 4], "one pixel down"),
        ([u32::MAX, 0, 2, 1], "x overflows u32"),
        ([0, u32::MAX, 1, 2], "y overflows u32"),
    ] {
        let atlas = atlas_with(page(), vec![entry("a", 0, rect, [4, 4], [0, 0])]);
        assert!(matches!(atlas.validate(), Err(AtlasError::Invalid(_))), "{why}");
    }
}

/// Catches the trim check being off by one: the trimmed rectangle may exactly fill the untrimmed
/// box (offset + size == source size) but not exceed it.
#[test]
fn trimmed_rects_must_fit_inside_the_source_box() {
    let page = || vec![image(32, 32)];
    let ok = atlas_with(page(), vec![entry("a", 0, [0, 0, 4, 2], [10, 5], [6, 3])]);
    assert!(ok.validate().is_ok(), "6 + 4 == 10 and 3 + 2 == 5");
    for (trim, why) in [([7, 3], "x"), ([6, 4], "y"), ([u32::MAX, 0], "x overflow")] {
        let bad = atlas_with(page(), vec![entry("a", 0, [0, 0, 4, 2], [10, 5], trim)]);
        assert!(matches!(bad.validate(), Err(AtlasError::Invalid(_))), "{why}");
    }
}

/// Catches validation checking a sprite against page 0 instead of the page it names, and a lookup
/// that fails on sprites of later pages.
#[test]
fn each_sprite_is_checked_against_its_own_page() {
    let pages = || vec![image(32, 32), image(8, 8)];
    let ok = atlas_with(
        pages(),
        vec![
            entry("big", 0, [16, 16, 16, 16], [16, 16], [0, 0]),
            entry("small", 1, [0, 0, 8, 8], [8, 8], [0, 0]),
        ],
    );
    assert!(ok.validate().is_ok());
    assert_eq!(ok.sprite("small").map(|s| s.page), Some(1));
    // The same rectangle that fits page 0 does not fit the 8x8 page 1.
    let bad = atlas_with(pages(), vec![entry("big", 1, [16, 16, 16, 16], [16, 16], [0, 0])]);
    assert!(matches!(bad.validate(), Err(AtlasError::Invalid(_))));
    let missing = atlas_with(pages(), vec![entry("big", 2, [0, 0, 1, 1], [1, 1], [0, 0])]);
    assert!(matches!(missing.validate(), Err(AtlasError::Invalid(_))));
}

/// Catches the binary-search lookup breaking on byte ordering (upper case before lower case, a
/// prefix before its extensions) or missing the first and last entries.
#[test]
fn lookup_finds_every_sprite_by_exact_name_in_byte_order() {
    let names = ["A", "B", "a", "a_1", "ab", "b", "\u{e9}toile"];
    let sprites: Vec<_> = names
        .iter()
        .map(|n| entry(n, 0, [0, 0, 1, 1], [1, 1], [0, 0]))
        .collect();
    let atlas = atlas_with(vec![image(2, 2)], sprites);
    assert!(atlas.validate().is_ok(), "the names above are in byte order");
    for n in names {
        assert_eq!(atlas.sprite(n).map(|s| s.name.as_str()), Some(n));
    }
    for miss in ["", "C", "a_", "aa", "abc", "etoile", "\u{e9}"] {
        assert!(atlas.sprite(miss).is_none(), "{miss:?}");
    }
}

/// Catches the sorted-unique rule accepting equal or descending neighbours (the lookup would then
/// miss sprites).
#[test]
fn cooked_names_must_be_strictly_ascending() {
    let two = |a: &str, b: &str| {
        atlas_with(
            vec![image(2, 2)],
            vec![
                entry(a, 0, [0, 0, 1, 1], [1, 1], [0, 0]),
                entry(b, 0, [1, 0, 1, 1], [1, 1], [0, 0]),
            ],
        )
    };
    assert!(two("a", "b").validate().is_ok());
    assert!(two("A", "a").validate().is_ok(), "names are case sensitive");
    assert!(matches!(two("a", "a").validate(), Err(AtlasError::Invalid(_))));
    assert!(matches!(two("b", "a").validate(), Err(AtlasError::Invalid(_))));
}

/// Catches image paths escaping the content root through spellings the simple checks miss.
#[test]
fn source_image_paths_cannot_escape_the_asset_root() {
    let parse = |path: &str| {
        AtlasSource::from_ron(&format!(
            r#"(version: 1, sprites: [(name: "a", image: {path:?})])"#
        ))
    };
    for bad in [
        "..",
        "a/..",
        "a\\..\\b",
        "a/b/../../..",
        "a:b",
        "//x",
        "\\\\server\\x",
        "C:/x.png",
    ] {
        assert!(
            matches!(parse(bad), Err(AtlasError::Invalid(_))),
            "{bad:?} must be rejected"
        );
    }
    for good in [
        "a.png",
        "dir/a.png",
        "./a.png",
        "a/./b.png",
        "..a.png",
        "a/..b/c.png",
        "a..b",
    ] {
        assert!(parse(good).is_ok(), "{good:?} must be accepted");
    }
}

/// Catches the image edge limit being off by one and a pixel buffer that is longer than declared.
#[test]
fn image_validation_checks_the_edge_limit_and_exact_length() {
    assert!(image(16_384, 1).validate().is_ok());
    assert!(image(1, 16_384).validate().is_ok());
    let too_wide = RgbaImage {
        width: 16_385,
        height: 1,
        rgba8: vec![0; 16_385 * 4],
    };
    assert!(too_wide.validate().is_err());
    let mut long = image(2, 2);
    long.rgba8.push(0);
    assert!(long.validate().is_err(), "17 bytes for a 2x2 image");
    let mut short = image(2, 2);
    short.rgba8.pop();
    assert!(short.validate().is_err());
    let zero_h = RgbaImage {
        width: 4,
        height: 0,
        rgba8: Vec::new(),
    };
    assert!(zero_h.validate().is_err());
}

/// Catches fields lost in the cooked encoding (trim, pivot, 9-slice order, page index) and any
/// damaged payload that panics or is accepted without being valid.
#[test]
fn cooked_payload_round_trips_every_field_and_survives_damage() {
    let mut panel = entry("panel", 1, [2, 2, 6, 6], [10, 8], [1, 2]);
    panel.pivot = [0.25, 0.75];
    panel.nine_slice = Some(NineSlice {
        left: 1,
        top: 2,
        right: 3,
        bottom: 1,
    });
    let atlas = atlas_with(
        vec![image(8, 8), image(16, 16)],
        vec![entry("a", 0, [1, 1, 2, 3], [4, 5], [0, 0]), panel],
    );
    assert!(atlas.validate().is_ok());
    let bytes = atlas.to_postcard().expect("encodes");
    let back = SpriteAtlas::from_postcard(&bytes).expect("decodes");
    assert_eq!(back, atlas);
    let p = back.sprite("panel").expect("panel");
    assert_eq!(
        (p.page, p.rect, p.source_size, p.trim_offset),
        (1, [2, 2, 6, 6], [10, 8], [1, 2])
    );
    assert_eq!(p.pivot, [0.25, 0.75]);
    let nine = p.nine_slice.expect("nine slice");
    assert_eq!((nine.left, nine.top, nine.right, nine.bottom), (1, 2, 3, 1));

    for len in 0..bytes.len() {
        assert!(SpriteAtlas::from_postcard(&bytes[..len]).is_err(), "prefix {len}");
    }
    for i in 0..bytes.len() {
        let mut damaged = bytes.clone();
        damaged[i] ^= 0x5A;
        if let Ok(decoded) = SpriteAtlas::from_postcard(&damaged) {
            assert!(decoded.validate().is_ok(), "byte {i}: accepted but invalid");
        }
    }
}

/// Catches GPU keys being reused (a reloaded page would keep its stale texture) and the pages not
/// being moved out of the atlas.
#[test]
fn every_loaded_page_gets_a_fresh_key_and_the_atlas_keeps_no_pixels() {
    let make = || {
        atlas_with(
            vec![image(2, 2), image(4, 4)],
            vec![entry("a", 0, [0, 0, 1, 1], [1, 1], [0, 0])],
        )
    };
    let one = LoadedAtlas::new(make());
    let two = LoadedAtlas::new(make());
    assert!(one.atlas.pages.is_empty(), "pixels moved into the GPU wrappers");
    assert_eq!(one.pages.len(), 2);
    let mut keys = vec![
        one.pages[0].key,
        one.pages[1].key,
        two.pages[0].key,
        two.pages[1].key,
    ];
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), 4);
    assert_eq!(one.pages[1].image.width, 4);
    assert!(one.atlas.sprite("a").is_some());
}

/// Catches a re-registered atlas keeping the old content or not bumping the revision the
/// renderer watches, and the failure record holding a stale generation.
#[test]
fn reinserting_replaces_content_and_failures_track_the_latest_generation() {
    let mut assets = SpriteAssets::default();
    assets.insert_atlas(
        "a",
        atlas_with(
            vec![image(2, 2)],
            vec![entry("x", 0, [0, 0, 1, 1], [1, 1], [0, 0])],
        ),
    );
    assets.insert_atlas(
        "a",
        atlas_with(
            vec![image(2, 2)],
            vec![entry("y", 0, [0, 0, 1, 1], [1, 1], [0, 0])],
        ),
    );
    assert_eq!(assets.revision, 2);
    let atlas = assets.atlas("a").expect("registered");
    assert!(atlas.atlas.sprite("x").is_none() && atlas.atlas.sprite("y").is_some());

    assets.record_failure("p", true, 3, "first".into());
    assert!(assets.failed_at("p", true, 3));
    assets.record_failure("p", true, 4, "second".into());
    assert!(!assets.failed_at("p", true, 3), "only generation 4 failed now");
    assert!(assets.failed_at("p", true, 4));
    assert_eq!(assets.failed["p"], (4, "second".to_string()));
    assert!(!assets.failed_at("other", true, 4));
}

/// Catches the component defaults drifting (they are the values every scene file omits).
#[test]
fn component_defaults_are_visible_white_and_unscaled() {
    let s = Sprite::default();
    assert!(s.visible && !s.flip_x && !s.flip_y);
    assert_eq!((s.color, s.size, s.anchor), ([1.0; 4], None, None));
    assert_eq!(Sprite::named("a", "n").source, SpriteSource::Named("n".into()));
    assert_eq!(
        Sprite::rect("a", [1, 2, 3, 4]).source,
        SpriteSource::Rect { rect: [1, 2, 3, 4] }
    );
    let m = TileMap::new("level");
    assert!(m.visible);
    assert_eq!(
        (m.map.as_str(), m.color, m.layer_z_step),
        ("level", [1.0; 4], 0.001)
    );
    let v = View2d::default();
    assert!(v.snap && v.pixel_perfect.is_none() && v.clear.is_none());
    assert_eq!((v.camera.pixels_per_unit, v.camera.rotation), (1.0, 0.0));
}
