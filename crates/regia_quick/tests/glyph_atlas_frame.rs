// SPDX-License-Identifier: MPL-2.0
//! The glyph atlas frame bracket (ADR-8832) in the quick layer (ADR-8930): a frame of immediate
//! text that needs more glyphs than the atlas pages hold never recycles a page its own quads
//! already point into. Oracle (the regia_ui `atlas_frame_pins` test's): two glyph quads of the
//! frame on the same page either share their rect (the same cached glyph) or do not overlap; a
//! page wiped under an earlier quad puts a later glyph over that quad's rect.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_quick::frame::{FrameParams, QuickRenderer, glyph_image_id};
use regia_quick::prelude::*;
use regia_render::{ImmediateBatch, ImmediateTexture};
use regia_ui::Localization;
use regia_ui::atlas::{MAX_PAGES, PAGE_SIZE};
use regia_ui::text::TextSystem;

const PARAMS: FrameParams = FrameParams {
    size: Vec2::new(640.0, 360.0),
    scale_factor: 1.0,
};

const CHARS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Twelve sizes of the same 62 characters: about 740 large glyphs, more than the pages hold,
/// while each size alone fits.
const SIZES: [u32; 12] = [100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144];

/// The mono page a glyph id names, if it is one.
fn mono_page(texture: ImmediateTexture) -> Option<u32> {
    let ImmediateTexture::Mask(id) = texture else {
        return None;
    };
    (0..MAX_PAGES).find(|page| glyph_image_id(0, *page) == id)
}

/// Every glyph quad of the batch: `(page, [x, y, w, h])` in atlas pixels.
fn glyph_rects(batch: &ImmediateBatch) -> Vec<(u32, [u32; 4])> {
    let px = |v: f32| (v * PAGE_SIZE as f32).round() as u32;
    let mut rects = Vec::new();
    for draw in batch.draws() {
        let Some(page) = mono_page(draw.state.texture) else {
            continue;
        };
        let indices = &batch.indices()[draw.indices.start as usize..draw.indices.end as usize];
        // A glyph is two triangles over four vertices: `[0, 1, 2, 0, 2, 3]`.
        for quad in indices.as_chunks::<6>().0 {
            let uvs: Vec<[f32; 2]> = quad.iter().map(|&i| batch.vertices()[i as usize].uv).collect();
            let u0 = uvs.iter().map(|uv| uv[0]).fold(f32::MAX, f32::min);
            let v0 = uvs.iter().map(|uv| uv[1]).fold(f32::MAX, f32::min);
            let u1 = uvs.iter().map(|uv| uv[0]).fold(f32::MIN, f32::max);
            let v1 = uvs.iter().map(|uv| uv[1]).fold(f32::MIN, f32::max);
            rects.push((page, [px(u0), px(v0), px(u1) - px(u0), px(v1) - px(v0)]));
        }
    }
    rects
}

/// Asserts no two different glyph rects of one page overlap.
fn assert_apart(rects: &[(u32, [u32; 4])], what: &str) {
    for (n, (page, a)) in rects.iter().enumerate() {
        for (other_page, b) in &rects[n + 1..] {
            if page != other_page || a == b {
                continue;
            }
            let apart =
                a[0] + a[2] <= b[0] || b[0] + b[2] <= a[0] || a[1] + a[3] <= b[1] || b[1] + b[3] <= a[1];
            assert!(
                apart,
                "{what}: page {page}: glyph rect {a:?} overlaps {b:?} in one frame"
            );
        }
    }
}

fn draw_sizes(draw: &Draw, sizes: &[u32]) {
    for size in sizes {
        draw.text(CHARS).font_size(*size).no_line_wrap();
    }
}

#[test]
fn a_frame_that_overflows_the_glyph_atlas_keeps_its_quads_on_their_own_glyphs() {
    let draw = Draw::new();
    let mut fonts = TextSystem::embedded_only();
    let localization = Localization::default();
    let mut renderer = QuickRenderer::new();
    let mut batch = ImmediateBatch::new();
    draw_sizes(&draw, &SIZES);
    renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    let rects = glyph_rects(&batch);
    assert!(!rects.is_empty(), "glyph quads were drawn");
    assert_apart(&rects, "one frame");
    assert!(
        renderer.glyph_atlas_thrash() > 0,
        "the frame overflowed the atlas (else this test proves nothing): {} glyph quads",
        rects.len()
    );
}

/// A text replayed from the shape cache emits quads without asking the atlas: its pages must
/// be pinned all the same, or a later text of the frame recycles them under it.
#[test]
fn a_cached_text_keeps_its_page_when_the_frame_overflows_after_it() {
    let draw = Draw::new();
    let mut fonts = TextSystem::embedded_only();
    let localization = Localization::default();
    let mut renderer = QuickRenderer::new();
    let mut batch = ImmediateBatch::new();
    draw.text("cached first").font_size(48).no_line_wrap();
    renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    let shapes = renderer.text_shape_count();
    draw.text("cached first").font_size(48).no_line_wrap();
    draw_sizes(&draw, &SIZES);
    renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    assert_eq!(
        renderer.text_shape_count(),
        shapes + SIZES.len() as u64,
        "the first text came from the shape cache"
    );
    assert_apart(&glyph_rects(&batch), "a cache hit, then an overflow");
    assert!(
        renderer.glyph_atlas_thrash() > 0,
        "the frame overflowed the atlas"
    );
}

/// A text that lost glyphs to a full atlas is not kept in the shape cache: the next frame,
/// with room again, draws all of it.
#[test]
fn a_text_cut_short_by_a_full_atlas_is_drawn_whole_next_frame() {
    let last = SIZES[SIZES.len() - 1];
    let localization = Localization::default();
    let whole = {
        let draw = Draw::new();
        let mut fonts = TextSystem::embedded_only();
        let mut batch = ImmediateBatch::new();
        draw.text(CHARS).font_size(last).no_line_wrap();
        QuickRenderer::new().build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
        glyph_rects(&batch).len()
    };
    assert_eq!(whole, CHARS.len(), "every character is a glyph quad");
    let draw = Draw::new();
    let mut fonts = TextSystem::embedded_only();
    let mut renderer = QuickRenderer::new();
    let mut batch = ImmediateBatch::new();
    draw_sizes(&draw, &SIZES);
    renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    assert!(
        renderer.glyph_atlas_thrash() > 0,
        "the frame overflowed the atlas"
    );
    draw.text(CHARS).font_size(last).no_line_wrap();
    renderer.build(&draw, PARAMS, Some((&mut fonts, &localization)), &mut batch);
    assert_eq!(
        glyph_rects(&batch).len(),
        whole,
        "the last text kept the glyphs the full frame refused"
    );
}
