// SPDX-License-Identifier: MPL-2.0
//! The UI font atlas as the glyph source of the renderer's debug labels (`specs/13` § 11,
//! ADR-7254).
//!
//! [`UiPlugin`](crate::UiPlugin) installs [`UiDebugLabelGlyphs`] into
//! [`regia_render::DebugLabelGlyphs`], so every [`regia_render::DebugText`] label draws with the
//! UI's shaping (cosmic-text over the embedded UI fonts) and its glyph atlas type
//! ([`GlyphAtlases`], R8 pages of [`PAGE_SIZE`] texels). The labels keep their own atlas pages:
//! the overlay's pages are recycled on the overlay's schedule, and a label must not lose a glyph
//! the overlay wiped in the same frame. Colour glyphs (emoji) are skipped.

use std::sync::Arc;

use regia_render::{DebugLabelGlyph, DebugLabelGlyphSource, DebugLabelPage};

use crate::atlas::{GlyphAtlases, PAGE_SIZE};
use crate::localization::Localization;
use crate::text::{Text, TextStyle, TextSystem};

/// The font size labels are shaped and rasterized at, physical pixels.
pub const LABEL_FONT_PX: f32 = 32.0;

/// The cap height of the UI fonts, as a share of the font size: a label of size `s` meters
/// (its cap height) is shaped at [`LABEL_FONT_PX`] and drawn at `s / (LABEL_FONT_PX ×
/// LABEL_CAP_HEIGHT_EM)` meters per pixel.
pub const LABEL_CAP_HEIGHT_EM: f32 = 0.7;

/// Laid-out labels kept between frames before the cache is cleared.
const LAYOUT_CACHE: usize = 256;

/// The debug-label glyph source over the UI's shaping and glyph atlas.
#[derive(Default)]
pub struct UiDebugLabelGlyphs {
    /// Built on the first label (the embedded fonts only: identical on every platform).
    text: Option<TextSystem>,
    atlas: GlyphAtlases,
    loc: Localization,
    /// Laid-out labels by text; cleared when the atlas recycles a page.
    layouts: std::collections::HashMap<String, Vec<DebugLabelGlyph>>,
    /// The mono pages as last copied, and the atlas state they were copied at.
    pages: Vec<DebugLabelPage>,
    copied_at: (usize, u64),
    generation: u64,
}

impl UiDebugLabelGlyphs {
    fn shape(&mut self, label: &str) -> Vec<DebugLabelGlyph> {
        let text = self.text.get_or_insert_with(TextSystem::embedded_only);
        let shaped = text.shape(
            &Text::new(label).with_style(TextStyle {
                font_px: LABEL_FONT_PX,
                ..TextStyle::default()
            }),
            1.0,
            None,
            &self.loc,
        );
        let k = 1.0 / (LABEL_FONT_PX * LABEL_CAP_HEIGHT_EM);
        let (half_w, half_h) = (shaped.size[0] * 0.5, shaped.size[1] * 0.5);
        let page = PAGE_SIZE as f32;
        let mut glyphs = Vec::with_capacity(shaped.glyphs.len());
        for glyph in &shaped.glyphs {
            let key = glyph.cache_key;
            let Some((place, offset)) = self.atlas.get_or_insert(key, || text.glyph_image(key)) else {
                continue;
            };
            if place.kind != 0 {
                continue;
            }
            let [rx, ry, w, h] = place.rect.map(|v| v as f32);
            let x = (glyph.pen[0] + offset[0]) as f32;
            let y = (glyph.pen[1] + offset[1]) as f32;
            glyphs.push(DebugLabelGlyph {
                page: place.page,
                // Pixels (Y down from the block's top) to cap heights (Y up from its centre).
                rect: [
                    (x - half_w) * k,
                    (half_h - y - h) * k,
                    (x + w - half_w) * k,
                    (half_h - y) * k,
                ],
                uv: [rx / page, ry / page, (rx + w) / page, (ry + h) / page],
            });
        }
        glyphs
    }

    /// Copies the mono pages again when the atlas changed since the last copy.
    fn refresh_pages(&mut self) {
        let state = (self.atlas.len(), self.atlas.wipes());
        if state == self.copied_at && !self.pages.is_empty() {
            return;
        }
        self.copied_at = state;
        self.generation += 1;
        self.pages = (0..self.atlas.page_count(0))
            .filter_map(|p| {
                let (data, bpp) = self.atlas.page_data(0, p)?;
                (bpp == 1).then(|| DebugLabelPage {
                    size: PAGE_SIZE,
                    texels: Arc::from(data),
                    generation: self.generation,
                })
            })
            .collect();
    }
}

impl DebugLabelGlyphSource for UiDebugLabelGlyphs {
    fn layout(&mut self, text: &str) -> Option<Vec<DebugLabelGlyph>> {
        let wipes = self.atlas.wipes();
        if let Some(glyphs) = self.layouts.get(text) {
            return Some(glyphs.clone());
        }
        if self.layouts.len() >= LAYOUT_CACHE {
            self.layouts.clear();
        }
        let glyphs = self.shape(text);
        if self.atlas.wipes() != wipes {
            // A recycled page invalidates the cached labels that sat on it.
            self.layouts.clear();
        }
        self.layouts.insert(text.to_owned(), glyphs.clone());
        self.refresh_pages();
        Some(glyphs)
    }

    fn page(&self, page: u32) -> Option<DebugLabelPage> {
        self.pages.get(page as usize).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_lays_out_on_an_atlas_page_with_coverage_under_its_glyphs() {
        let mut source = UiDebugLabelGlyphs::default();
        let glyphs = source.layout("Hi 7").expect("laid out");
        assert_eq!(glyphs.len(), 3, "H, i and 7 (the space has no bitmap)");
        for g in &glyphs {
            let page = source.page(g.page).expect("its page");
            assert!(page.size == PAGE_SIZE && page.texels.len() == (PAGE_SIZE * PAGE_SIZE) as usize);
            // The glyph's texel rectangle on the page holds coverage.
            let [u0, v0, u1, v1] = g.uv.map(|t| (t * PAGE_SIZE as f32) as usize);
            let covered = (v0..v1)
                .flat_map(|y| (u0..u1).map(move |x| (x, y)))
                .filter(|&(x, y)| page.texels[y * PAGE_SIZE as usize + x] > 128)
                .count();
            assert!(covered > 0, "glyph {g:?} has no coverage on its page");
            assert!(g.rect[2] > g.rect[0] && g.rect[3] > g.rect[1], "{g:?}");
        }
        // Centred: the glyphs straddle x = 0; an "H" is about one cap height tall.
        assert!(glyphs[0].rect[0] < 0.0 && glyphs[2].rect[2] > 0.0);
        let h = glyphs[0].rect[3] - glyphs[0].rect[1];
        assert!((0.8..1.3).contains(&h), "H height {h} cap heights");
        // A second layout of the same text reuses the pages (same generation).
        let generation = source.page(0).expect("page").generation;
        source.layout("Hi 7").expect("again");
        assert_eq!(source.page(0).expect("page").generation, generation);
    }

    #[test]
    fn the_ui_plugin_installs_the_source() {
        let mut app = regia_app::prelude::App::new();
        app.add_plugins(crate::UiPlugin::code_only());
        assert!(
            app.world()
                .get_resource::<regia_render::DebugLabelGlyphs>()
                .is_some_and(regia_render::DebugLabelGlyphs::is_installed)
        );
    }
}
