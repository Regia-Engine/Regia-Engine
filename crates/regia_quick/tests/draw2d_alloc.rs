// SPDX-License-Identifier: MPL-2.0
//! A steady `Draw2dList` frame allocates nothing (P-05): once the instance list, the glyph atlas
//! and the staging buffers have grown, building the same HUD frame again (boxes, outlines,
//! segments, an image and a shaped text run whose glyphs are cached) and handing it to the GPU
//! staging ([`Draw2dList::hand_over`]) allocates nothing on the calling thread from frame 3 on.
//!
//! Counted with a thread-local counting global allocator (other test threads and the harness do
//! not count). CPU only: no device, no wall clock. The text is shaped once, before the frames, as
//! a HUD caches its shaped runs.

#![allow(unsafe_code)] // the counting `GlobalAlloc` (ADR-5631), as in `view_visibility_alloc.rs`
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::alloc::{GlobalAlloc, Layout, System};

use regia_core::math::{Color, Vec2};
use regia_quick::draw2d::{Draw2dFrame, Draw2dImage, Draw2dList, PxRect};
use regia_ui::Localization;
use regia_ui::text::{Text, TextSpan, TextStyle, TextSystem};

struct Counting;

thread_local! {
    static THREAD_ALLOCS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn thread_allocs() -> u64 {
    THREAD_ALLOCS.with(|c| c.get())
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

#[test]
fn draw2d_steady_frame_allocates_nothing() {
    let mut fonts = TextSystem::embedded_only();
    let loc = Localization::default();
    let text = Text {
        spans: vec![TextSpan::lit("AMMO 24 / 120").with_style(TextStyle {
            font_px: 16.0,
            ..Default::default()
        })],
        ..Default::default()
    };
    let shaped = fonts.shape(&text, 1.0, None, &loc);
    let mut list = Draw2dList::new();
    let mut staged = Draw2dFrame::default();
    let mut per_frame = Vec::new();
    for frame in 0..8u32 {
        let before = thread_allocs();
        list.begin_frame();
        for i in 0..64 {
            let x = (i % 16) as f32 * 30.0;
            let y = (i / 16) as f32 * 30.0;
            list.push_box(PxRect::new(x, y, 24.0, 24.0), Color::rgb(0.2, 0.4, 0.8), 4.0);
            list.push_outline(PxRect::new(x, y, 24.0, 24.0), Color::WHITE, 4.0, 1.0);
            list.push_segment(Vec2::new(x, y), Vec2::new(x + 24.0, y + 24.0), 1.5, Color::RED);
        }
        list.set_clip(Some(PxRect::new(0.0, 0.0, 200.0, 100.0)));
        list.push_image(
            Draw2dImage(1),
            PxRect::new(4.0, 4.0, 32.0, 32.0),
            [0.0, 0.0, 1.0, 1.0],
            Color::WHITE,
        );
        list.set_clip(None);
        list.push_text(&mut fonts, &shaped, Vec2::new(10.0, 200.0), Color::WHITE);
        list.end_frame();
        list.hand_over(&mut staged);
        per_frame.push(thread_allocs() - before);
        assert_eq!(staged.instances().len(), list.instances().len(), "frame {frame}");
    }
    assert!(per_frame[0] > 0, "the first frame grows the lists: {per_frame:?}");
    assert!(
        per_frame[2..].iter().all(|n| *n == 0),
        "allocations per frame (frame 3 on must be 0): {per_frame:?}"
    );
    assert!(list.stats().glyphs >= 9, "{:?}", list.stats());
}

/// P-06: hashing a steady list per tile and finding its (empty) dirty runs allocates nothing once
/// the tracker's buffers have grown.
#[test]
fn draw2d_tiles_steady_update_allocates_nothing() {
    use regia_quick::draw2d::Draw2dTiles;
    let mut list = Draw2dList::new();
    list.begin_frame();
    for i in 0..200 {
        let x = (i % 20) as f32 * 90.0;
        let y = (i / 20) as f32 * 100.0;
        list.push_box(PxRect::new(x, y, 60.0, 40.0), Color::rgb(0.2, 0.4, 0.8), 4.0);
        list.push_segment(Vec2::new(x, y), Vec2::new(x + 80.0, y + 90.0), 2.0, Color::RED);
    }
    list.end_frame();
    let mut tiles = Draw2dTiles::default();
    let mut per_frame = Vec::new();
    for _ in 0..6 {
        let before = thread_allocs();
        let runs = tiles.update(list.instances(), 1920, 1080).len();
        per_frame.push((thread_allocs() - before, runs));
    }
    assert!(per_frame[0].0 > 0 && per_frame[0].1 > 0, "{per_frame:?}");
    assert!(
        per_frame[1..].iter().all(|&(a, r)| a == 0 && r == 0),
        "steady updates allocate nothing and redraw nothing: {per_frame:?}"
    );
}
