// SPDX-License-Identifier: MPL-2.0
//! ADR-8832 end to end on the CPU: a UI frame that needs more glyphs than the atlas pages hold
//! never recycles a page its own quads already point into. Oracle: two unclipped glyph quads of
//! the frame that sample the same page either share their rect (the same cached glyph) or do not
//! overlap; a page wiped under an earlier quad would put a later glyph over its rect.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::prelude::*;
use regia_ui::atlas::PAGE_SIZE;
use regia_ui::prelude::*;
use regia_ui::render::SharedUiRenderer;
use regia_ui::{TextureRef, UiQuad};

fn ui_app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app
}

/// The atlas rect of an unclipped glyph quad: `(page, [x, y, w, h])`.
fn glyph_rect(q: &UiQuad) -> Option<(u32, [u32; 4])> {
    let TextureRef::MonoAtlas(page) = q.texture else {
        return None;
    };
    let px = |v: f32| (v * PAGE_SIZE as f32).round() as u32;
    let rect = [
        px(q.uv[0]),
        px(q.uv[1]),
        px(q.uv[2]) - px(q.uv[0]),
        px(q.uv[3]) - px(q.uv[1]),
    ];
    // A clipped quad has a cut UV span; only whole glyphs are compared.
    let whole = (q.size[0] - rect[2] as f32).abs() < 0.01 && (q.size[1] - rect[3] as f32).abs() < 0.01;
    whole.then_some((page, rect))
}

#[test]
fn a_frame_that_overflows_the_glyph_atlas_keeps_its_quads_on_their_own_glyphs() {
    let mut app = ui_app();
    let world = app.world_mut();
    let root = world
        .spawn((
            UiRoot::default(),
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id();
    // Ten sizes of the same 62 characters: about 620 large glyphs, far more than 8 pages hold.
    let chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    for size in 0..10 {
        let style = TextStyle {
            font_px: 120.0 + 20.0 * size as f32,
            ..TextStyle::default()
        };
        let node = world
            .spawn((
                UiNode,
                Text::new(chars).with_style(style).with_wrap(WrapMode::None),
            ))
            .id();
        world.entity_mut(root).add_child(node);
    }
    app.update();
    let renderer = app.world().resource::<SharedUiRenderer>();
    let mut guard = renderer.lock();
    let rects: Vec<(u32, [u32; 4])> = guard.quads().iter().filter_map(glyph_rect).collect();
    assert!(!rects.is_empty(), "glyph quads were extracted");
    for (n, (page, a)) in rects.iter().enumerate() {
        for (other_page, b) in &rects[n + 1..] {
            if page != other_page || a == b {
                continue;
            }
            let apart =
                a[0] + a[2] <= b[0] || b[0] + b[2] <= a[0] || a[1] + a[3] <= b[1] || b[1] + b[3] <= a[1];
            assert!(apart, "page {page}: glyph rect {a:?} overlaps {b:?} in one frame");
        }
    }
    let atlas = guard.atlas_mut();
    assert!(
        atlas.thrash() > 0,
        "the frame overflowed the atlas (else this test proves nothing): {} glyph quads",
        rects.len()
    );
}
