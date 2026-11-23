// SPDX-License-Identifier: MPL-2.0
//! The glyph-source hook of the debug labels (`specs/13` § 11 "text via UI font atlas", ADR-7254).
//!
//! [`crate::DebugText`] labels draw from a glyph atlas when a [`DebugLabelGlyphSource`] is
//! installed in the [`DebugLabelGlyphs`] resource: the source lays the text out and hands back
//! one quad per glyph with its rectangle on an atlas page, and the gizmo pass draws those quads
//! facing the camera, sampling the page ([`crate::gizmos::GizmoLabelBatch`]). `regia_ui` installs
//! its font atlas as the source (its `UiPlugin`); `regia_render` never depends on `regia_ui`.
//! Without a source, or for a label the source cannot lay out, the gizmo stroke font draws the
//! label as before.

use std::sync::Arc;

use regia_app::prelude::*;

/// One glyph of a laid-out label.
///
/// `rect` is in units of the label's cap height, origin at the label's centre, +X right and +Y
/// up in the label plane: a label of size `s` meters places the glyph at `rect × s`. `uv` is the
/// glyph's rectangle on its atlas page (0..1, V down).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DebugLabelGlyph {
    /// The atlas page the glyph is on ([`DebugLabelGlyphSource::page`]).
    pub page: u32,
    /// `[x0, y0, x1, y1]`: left, bottom, right, top, in cap heights.
    pub rect: [f32; 4],
    /// `[u0, v0, u1, v1]`: the glyph's texels on the page (V down: `v0` is the top edge).
    pub uv: [f32; 4],
}

/// One atlas page as the gizmo pass samples it: single-channel coverage (R8), `size × size`
/// texels, row-major.
#[derive(Clone, PartialEq)]
pub struct DebugLabelPage {
    /// Page edge in texels.
    pub size: u32,
    /// Coverage texels, `size × size` bytes.
    pub texels: Arc<[u8]>,
    /// Changes whenever the texels change; the pass re-uploads a page whose generation moved.
    pub generation: u64,
}

impl std::fmt::Debug for DebugLabelPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugLabelPage")
            .field("size", &self.size)
            .field("texels", &self.texels.len())
            .field("generation", &self.generation)
            .finish()
    }
}

impl DebugLabelPage {
    /// Whether the page holds `size × size` texels (a malformed page is not drawn).
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.size > 0 && self.texels.len() == (self.size as usize) * (self.size as usize)
    }
}

/// A glyph atlas the debug labels draw from. `regia_ui` implements it over its font atlas.
pub trait DebugLabelGlyphSource: Send + Sync + 'static {
    /// The glyphs of `text` (printable text; `\n` starts a line), or `None` when this source
    /// cannot lay it out (the stroke font draws it then). An empty list draws nothing.
    fn layout(&mut self, text: &str) -> Option<Vec<DebugLabelGlyph>>;

    /// Atlas page `page` as it is after the last [`Self::layout`], or `None` when it does not
    /// exist.
    fn page(&self, page: u32) -> Option<DebugLabelPage>;
}

/// A laid-out label: its glyphs, and each page they sit on with its index (first-use order).
pub type DebugLabelLayout = (Vec<DebugLabelGlyph>, Vec<(u32, DebugLabelPage)>);

/// The installed glyph source of the debug labels (none by default: stroke font).
#[derive(Resource, Default)]
pub struct DebugLabelGlyphs {
    source: Option<Box<dyn DebugLabelGlyphSource>>,
}

impl DebugLabelGlyphs {
    /// Installs `source`, replacing any earlier one.
    pub fn install(&mut self, source: Box<dyn DebugLabelGlyphSource>) {
        self.source = Some(source);
    }

    /// Removes the source: labels draw with the stroke font again.
    pub fn uninstall(&mut self) {
        self.source = None;
    }

    /// Whether a source is installed.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.source.is_some()
    }

    /// The source's glyphs for `text` and the pages they sit on (one entry per page, in first-use
    /// order), or `None` when no source is installed, it refuses the text, or a page it names is
    /// missing or malformed: the caller then draws the stroke font.
    pub fn layout(&mut self, text: &str) -> Option<DebugLabelLayout> {
        let source = self.source.as_mut()?;
        let glyphs = source.layout(text)?;
        let mut pages: Vec<(u32, DebugLabelPage)> = Vec::new();
        for glyph in &glyphs {
            if pages.iter().any(|(p, _)| *p == glyph.page) {
                continue;
            }
            let page = source.page(glyph.page).filter(DebugLabelPage::is_valid)?;
            pages.push((glyph.page, page));
        }
        Some((glyphs, pages))
    }
}

/// Installs `source` as the debug labels' glyph atlas in `app` ([`DebugLabelGlyphs::install`]).
pub fn install_debug_label_glyphs(app: &mut App, source: Box<dyn DebugLabelGlyphSource>) {
    app.world_mut()
        .get_resource_or_init::<DebugLabelGlyphs>()
        .install(source);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A one-page source whose every character is one glyph quad over the page's top-left
    /// quarter.
    pub(crate) struct QuarterPage {
        pub(crate) page: DebugLabelPage,
    }

    impl QuarterPage {
        pub(crate) fn new(fill: u8) -> Self {
            Self {
                page: DebugLabelPage {
                    size: 8,
                    texels: vec![fill; 64].into(),
                    generation: 1,
                },
            }
        }
    }

    impl DebugLabelGlyphSource for QuarterPage {
        fn layout(&mut self, text: &str) -> Option<Vec<DebugLabelGlyph>> {
            let n = text.chars().count() as f32;
            Some(
                text.chars()
                    .enumerate()
                    .map(|(i, _)| {
                        let x = i as f32 - n * 0.5;
                        DebugLabelGlyph {
                            page: 0,
                            rect: [x, -0.5, x + 0.8, 0.5],
                            uv: [0.0, 0.0, 0.5, 0.5],
                        }
                    })
                    .collect(),
            )
        }

        fn page(&self, page: u32) -> Option<DebugLabelPage> {
            (page == 0).then(|| self.page.clone())
        }
    }

    #[test]
    fn without_a_source_nothing_lays_out() {
        let mut glyphs = DebugLabelGlyphs::default();
        assert!(!glyphs.is_installed());
        assert!(glyphs.layout("A").is_none());
    }

    #[test]
    fn a_source_lays_out_and_names_its_pages_once() {
        let mut glyphs = DebugLabelGlyphs::default();
        glyphs.install(Box::new(QuarterPage::new(255)));
        let (quads, pages) = glyphs.layout("ABC").expect("laid out");
        assert_eq!(quads.len(), 3);
        assert_eq!(pages.len(), 1, "three glyphs on one page");
        assert_eq!(pages[0].0, 0);
        glyphs.uninstall();
        assert!(glyphs.layout("ABC").is_none());
    }

    #[test]
    fn a_malformed_page_falls_back() {
        let mut source = QuarterPage::new(255);
        source.page.texels = vec![0u8; 3].into();
        let mut glyphs = DebugLabelGlyphs::default();
        glyphs.install(Box::new(source));
        assert!(
            glyphs.layout("A").is_none(),
            "a page of the wrong size is not drawn"
        );
    }
}
