// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): the glyph atlas under pressure: oversize and malformed images,
//! page recycling (LRU), dirty-page bookkeeping, colour premultiplication and a seeded packing
//! property. CPU only, no GPU, no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use cosmic_text as ct;
use regia_test_utils::fuzz::FuzzRng;
use regia_ui::atlas::{GlyphAtlases, MAX_PAGES, PAGE_SIZE};

fn key(i: u32) -> ct::CacheKey {
    ct::CacheKey {
        font_id: ct::fontdb::ID::dummy(),
        glyph_id: i as u16,
        font_size_bits: 18.0f32.to_bits(),
        x_bin: ct::SubpixelBin::Zero,
        y_bin: ct::SubpixelBin::Zero,
        font_weight: ct::fontdb::Weight::NORMAL,
        flags: ct::CacheKeyFlags::empty(),
    }
}

fn mask(w: u32, h: u32, fill: u8) -> ct::SwashImage {
    let mut image = ct::SwashImage::new();
    image.placement.width = w;
    image.placement.height = h;
    image.content = ct::SwashContent::Mask;
    image.data = vec![fill; (w * h) as usize];
    image
}

fn color(w: u32, h: u32, rgba: [u8; 4]) -> ct::SwashImage {
    let mut image = ct::SwashImage::new();
    image.placement.width = w;
    image.placement.height = h;
    image.content = ct::SwashContent::Color;
    image.data = rgba.repeat((w * h) as usize);
    image
}

#[test]
fn unusable_images_are_refused_without_corrupting_state() {
    let mut atlas = GlyphAtlases::default();
    assert!(
        atlas.get_or_insert(key(1), || None).is_none(),
        "rasterizer gave nothing"
    );
    assert!(
        atlas.get_or_insert(key(2), || Some(mask(0, 8, 1))).is_none(),
        "zero width"
    );
    assert!(
        atlas.get_or_insert(key(3), || Some(mask(8, 0, 1))).is_none(),
        "zero height"
    );
    assert!(
        atlas
            .get_or_insert(key(4), || Some(mask(PAGE_SIZE + 1, 4, 1)))
            .is_none(),
        "wider than a page"
    );
    assert!(
        atlas
            .get_or_insert(key(5), || Some(mask(4, PAGE_SIZE + 1, 1)))
            .is_none(),
        "taller than a page"
    );
    // A bitmap shorter than its placement says is refused, not read out of bounds.
    let mut short = mask(16, 16, 9);
    short.data.truncate(100);
    assert!(atlas.get_or_insert(key(6), || Some(short)).is_none());
    assert!(atlas.is_empty(), "nothing was cached");
    // The atlas still works afterwards.
    let (place, _) = atlas.get_or_insert(key(7), || Some(mask(8, 8, 200))).unwrap();
    assert_eq!(place.rect, [0, 0, 8, 8]);
    assert_eq!(atlas.len(), 1);
}

#[test]
fn a_full_page_glyph_fits_and_the_bitmap_lands_in_the_page() {
    let mut atlas = GlyphAtlases::default();
    let (place, _) = atlas
        .get_or_insert(key(1), || Some(mask(PAGE_SIZE, PAGE_SIZE, 77)))
        .unwrap();
    assert_eq!(place.rect, [0, 0, PAGE_SIZE, PAGE_SIZE]);
    let (data, bpp) = atlas.page_data(0, place.page).unwrap();
    assert_eq!(bpp, 1);
    assert_eq!(data.len(), (PAGE_SIZE * PAGE_SIZE) as usize);
    assert!(data.iter().all(|&b| b == 77));
    assert!(atlas.page_data(0, 99).is_none());
    assert!(atlas.page_data(1, 0).is_none(), "no colour page yet");
}

#[test]
fn bitmap_offset_is_left_and_negated_top() {
    let mut atlas = GlyphAtlases::default();
    let mut image = mask(4, 4, 1);
    image.placement.left = -3;
    image.placement.top = 7;
    let (_, offset) = atlas.get_or_insert(key(1), || Some(image)).unwrap();
    assert_eq!(offset, [-3, -7]);
    // The cached lookup returns the same offset without rasterizing again.
    let (_, again) = atlas
        .get_or_insert(key(1), || panic!("must not rasterize twice"))
        .unwrap();
    assert_eq!(again, [-3, -7]);
}

#[test]
fn the_least_recently_used_page_is_recycled_and_a_hit_protects_a_page() {
    let mut atlas = GlyphAtlases::default();
    // One full-page glyph per page: eight pages are all there is.
    for i in 0..MAX_PAGES {
        let (place, _) = atlas
            .get_or_insert(key(i), || Some(mask(PAGE_SIZE, PAGE_SIZE, 10 + i as u8)))
            .unwrap();
        assert_eq!(place.page, i);
    }
    assert_eq!(atlas.page_count(0), MAX_PAGES);
    assert_eq!(atlas.wipes(), 0);
    // Touch the oldest glyph: page 0 is no longer the least recently used.
    assert!(atlas.get_or_insert(key(0), || None).is_some());
    let (place, _) = atlas
        .get_or_insert(key(100), || Some(mask(PAGE_SIZE, PAGE_SIZE, 99)))
        .unwrap();
    assert_eq!(place.page, 1, "page 1 was the least recently used");
    assert_eq!(atlas.wipes(), 1);
    assert_eq!(atlas.page_count(0), MAX_PAGES, "the page budget holds");
    assert!(
        atlas.get_or_insert(key(1), || None).is_none(),
        "its old glyph is gone"
    );
    assert!(
        atlas.get_or_insert(key(0), || None).is_some(),
        "the touched glyph survived"
    );
    assert!(atlas.get_or_insert(key(2), || None).is_some());
    let (data, _) = atlas.page_data(0, 1).unwrap();
    assert!(
        data.iter().all(|&b| b == 99),
        "the recycled page holds only the new glyph"
    );
    // Every surviving placement still points at a page that holds its glyph's pixels.
    assert_eq!(atlas.len(), MAX_PAGES as usize);
}

#[test]
fn dirty_pages_are_reported_once_and_cleared_by_take() {
    let mut atlas = GlyphAtlases::default();
    atlas.get_or_insert(key(1), || Some(mask(8, 8, 1))).unwrap();
    atlas.get_or_insert(key(2), || Some(mask(8, 8, 1))).unwrap();
    atlas
        .get_or_insert(key(3), || Some(color(8, 8, [1, 2, 3, 255])))
        .unwrap();
    let mut dirty = atlas.take_dirty_pages();
    dirty.sort_unstable();
    assert_eq!(dirty, [(0, 0), (1, 0)], "one entry per page, not per glyph");
    assert!(atlas.take_dirty_pages().is_empty());
    // A cache hit changes no pixels.
    atlas.get_or_insert(key(1), || None).unwrap();
    assert!(atlas.take_dirty_pages().is_empty());
}

#[test]
fn invalidate_page_drops_placements_and_marks_the_page_dirty_once() {
    let mut atlas = GlyphAtlases::default();
    atlas.get_or_insert(key(1), || Some(mask(8, 8, 1))).unwrap();
    atlas
        .get_or_insert(key(2), || Some(color(8, 8, [9, 9, 9, 255])))
        .unwrap();
    let _ = atlas.take_dirty_pages();
    atlas.invalidate_page(0, 0);
    atlas.invalidate_page(0, 0);
    assert!(
        atlas.get_or_insert(key(1), || None).is_none(),
        "mono placement dropped"
    );
    assert!(
        atlas.get_or_insert(key(2), || None).is_some(),
        "colour page untouched"
    );
    assert_eq!(atlas.take_dirty_pages(), [(0, 0)]);
    // Invalidating a page that never existed is harmless.
    atlas.invalidate_page(1, 40);
    assert_eq!(atlas.len(), 1);
}

#[test]
fn color_glyphs_are_premultiplied_in_linear_light() {
    let mut atlas = GlyphAtlases::default();
    let pixels = [
        [255u8, 255, 255, 255], // opaque stays as is
        [255, 255, 255, 128],   // half alpha: linear 0.5 re-encoded
        [255, 0, 0, 0],         // transparent loses its color
        [0, 0, 0, 77],          // black stays black
    ];
    let mut image = color(4, 1, [0; 4]);
    image.data = pixels.concat();
    let (place, _) = atlas.get_or_insert(key(1), || Some(image)).unwrap();
    let (data, bpp) = atlas.page_data(1, place.page).unwrap();
    assert_eq!(bpp, 4);
    let px = |x: usize| &data[x * 4..x * 4 + 4];
    assert_eq!(px(0), [255, 255, 255, 255]);
    // sRGB-encode(0.5019...) = 0.7352... -> 187.5 -> 188 (rounded half up).
    assert!(
        px(1)[0].abs_diff(188) <= 1 && px(1)[0] == px(1)[1] && px(1)[1] == px(1)[2],
        "{:?}",
        px(1)
    );
    assert_eq!(px(1)[3], 128, "alpha is never altered");
    assert_eq!(px(2), [0, 0, 0, 0]);
    assert_eq!(px(3), [0, 0, 0, 77]);
}

#[test]
fn seeded_packing_never_overlaps_or_leaves_the_page() {
    let mut rng = FuzzRng::new(0x5EED_A71A);
    let mut atlas = GlyphAtlases::default();
    let mut keys = Vec::new();
    for i in 0..3000u32 {
        let w = 1 + rng.below(70) as u32;
        let h = 1 + rng.below(70) as u32;
        let k = key(i);
        if atlas
            .get_or_insert(k, || Some(mask(w, h, 1 + (i % 250) as u8)))
            .is_some()
        {
            keys.push(k);
        }
    }
    assert!(atlas.wipes() > 0, "3000 glyphs of this size overflow eight pages");
    assert!(atlas.page_count(0) <= MAX_PAGES);
    // Live placements: in bounds, disjoint per page.
    let live: Vec<_> = keys
        .iter()
        .filter_map(|k| atlas.get_or_insert(*k, || None).map(|(p, _)| p))
        .collect();
    assert_eq!(live.len(), atlas.len());
    for (n, a) in live.iter().enumerate() {
        let [x, y, w, h] = a.rect;
        assert!(x + w <= PAGE_SIZE && y + h <= PAGE_SIZE, "{a:?}");
        for b in &live[n + 1..] {
            if a.kind == b.kind && a.page == b.page {
                let r = b.rect;
                let apart = x + w <= r[0] || r[0] + r[2] <= x || y + h <= r[1] || r[1] + r[3] <= y;
                assert!(apart, "overlap: {a:?} vs {b:?}");
            }
        }
    }
}

#[test]
fn the_same_inserts_give_the_same_layout() {
    let run = || {
        let mut rng = FuzzRng::new(42);
        let mut atlas = GlyphAtlases::default();
        let mut out = Vec::new();
        for i in 0..400u32 {
            let (w, h) = (1 + rng.below(60) as u32, 1 + rng.below(60) as u32);
            out.push(atlas.get_or_insert(key(i), || Some(mask(w, h, 5))));
        }
        (out, atlas.wipes())
    };
    assert_eq!(run(), run());
}
