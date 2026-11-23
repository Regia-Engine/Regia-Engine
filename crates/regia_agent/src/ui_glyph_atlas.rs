// SPDX-License-Identifier: MPL-2.0
//! `ui.glyph_atlas`: the UI glyph atlas's pages, glyphs, recycles and thrash (book
//! `agents/ui-glyph-atlas.md`, ADR-8832). Thrash above 0 means a frame needed more distinct glyphs
//! than the atlas pages hold, and the ones that did not fit were not drawn that frame.

use regia_ecs::prelude::*;
use regia_ui::atlas::{MAX_PAGES, PAGE_SIZE};
use regia_ui::render::SharedUiRenderer;
use serde_json::{Value, json};

use crate::protocol::{RpcError, UNSUPPORTED};

/// `ui.glyph_atlas {}`.
pub(crate) fn glyph_atlas(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(renderer) = world.get_resource::<SharedUiRenderer>() else {
        return Err(RpcError::new(
            UNSUPPORTED,
            "ui_not_installed",
            "this app has no UI renderer",
            "the app needs regia_ui::UiPlugin",
        ));
    };
    let mut guard = renderer.lock();
    let atlas = guard.atlas_mut();
    Ok(json!({
        "page_size": PAGE_SIZE,
        "max_pages": MAX_PAGES,
        "mono_pages": atlas.page_count(0),
        "color_pages": atlas.page_count(1),
        "glyphs": atlas.len(),
        "wipes": atlas.wipes(),
        "thrash": atlas.thrash(),
        "thrash_frames": atlas.thrash_frames(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic_text as ct;
    use regia_ui::render::UiRenderer;

    fn key(i: u16) -> ct::CacheKey {
        ct::CacheKey {
            font_id: ct::fontdb::ID::dummy(),
            glyph_id: i,
            font_size_bits: 18.0f32.to_bits(),
            x_bin: ct::SubpixelBin::Zero,
            y_bin: ct::SubpixelBin::Zero,
            font_weight: ct::fontdb::Weight::NORMAL,
            flags: ct::CacheKeyFlags::empty(),
        }
    }

    fn page_glyph() -> ct::SwashImage {
        let mut image = ct::SwashImage::new();
        image.placement.width = PAGE_SIZE;
        image.placement.height = PAGE_SIZE;
        image.content = ct::SwashContent::Mask;
        image.data = vec![9; (PAGE_SIZE * PAGE_SIZE) as usize];
        image
    }

    #[test]
    fn reports_pages_glyphs_and_thrash() {
        let mut world = World::new();
        let err = glyph_atlas(&mut world, &json!({})).expect_err("no renderer");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("ui_not_installed")
        );
        world.insert_resource(SharedUiRenderer(UiRenderer::new_handle()));
        let out = glyph_atlas(&mut world, &json!({})).expect("empty atlas");
        assert_eq!(
            (out["glyphs"].clone(), out["thrash"].clone()),
            (json!(0), json!(0))
        );
        {
            let renderer = world.resource::<SharedUiRenderer>();
            let mut guard = renderer.lock();
            let atlas = guard.atlas_mut();
            // One frame that needs a page more than the atlas has: one glyph is refused.
            atlas.begin_frame();
            for i in 0..=MAX_PAGES as u16 {
                let _ = atlas.get_or_insert(key(i), || Some(page_glyph()));
            }
            atlas.end_frame();
        }
        let out = glyph_atlas(&mut world, &json!({})).expect("atlas");
        assert_eq!(out["mono_pages"], json!(MAX_PAGES));
        assert_eq!(out["color_pages"], json!(0));
        assert_eq!(out["glyphs"], json!(MAX_PAGES));
        assert_eq!(out["thrash"], json!(1));
        assert_eq!(out["thrash_frames"], json!(1));
        assert_eq!(out["wipes"], json!(0));
        assert_eq!(out["page_size"], json!(PAGE_SIZE));
    }
}
