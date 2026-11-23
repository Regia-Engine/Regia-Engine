// SPDX-License-Identifier: MPL-2.0
//! Render-out HUD seam: lets a capture path (the agent's offscreen screenshot, a video render-out
//! host) get the game UI laid out for the *output* resolution and orientation instead of the
//! window's, without `regia_render` or the agent depending on `regia_ui`.
//!
//! `regia_ui` installs a [`RenderOutHud`] when its plugin is built. A capture calls
//! [`RenderOutHud::begin`] (UI laid out and extracted for the output), draws the overlay layers
//! into the output texture, then [`RenderOutHud::end`] (the window layout is restored).

use std::sync::Arc;

use regia_app::prelude::*;

/// Which UI layout a render-out uses. Hosts author one UI root per layout; the engine renders
/// the chosen one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HudLayout {
    /// Landscape for width ≥ height, portrait otherwise.
    #[default]
    Auto,
    /// The 16:9 (or any landscape) layout.
    Landscape,
    /// The native 9:16 (or any portrait) layout.
    Portrait,
}

impl HudLayout {
    /// Parses `"auto"`, `"landscape"` / `"16:9"` or `"portrait"` / `"9:16"`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "auto" => Some(Self::Auto),
            "landscape" | "16:9" => Some(Self::Landscape),
            "portrait" | "9:16" => Some(Self::Portrait),
            _ => None,
        }
    }
}

/// The output a render-out draws the HUD for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOutSpec {
    /// Width in pixels of the image the HUD is drawn into (the *render* size: output width ×
    /// [`Self::supersample`]).
    pub width: u32,
    /// Height in pixels of the image the HUD is drawn into.
    pub height: u32,
    /// The layout to render.
    pub layout: HudLayout,
    /// Supersampling factor (1 = none): the image is `supersample`× the delivered output and is
    /// averaged down afterwards. The HUD is laid out as for the delivered output, at
    /// `supersample`× the density: constant-pixel-size roots grow by the factor so the HUD matches
    /// the unsupersampled image.
    pub supersample: u32,
}

impl RenderOutSpec {
    /// A spec without supersampling.
    #[must_use]
    pub fn new(width: u32, height: u32, layout: HudLayout) -> Self {
        Self {
            width,
            height,
            layout,
            supersample: 1,
        }
    }

    /// The same spec drawn `factor`× larger (`width`/`height` are the delivered output size).
    #[must_use]
    pub fn supersampled(self, factor: u32) -> Self {
        let factor = factor.max(1);
        Self {
            width: self.width.saturating_mul(factor),
            height: self.height.saturating_mul(factor),
            supersample: factor,
            ..self
        }
    }
}

/// A hook run with exclusive world access.
pub type RenderOutFn = Arc<dyn Fn(&mut World, &RenderOutSpec) + Send + Sync>;

/// The UI side of render-out, installed by `regia_ui`'s plugin.
#[derive(Resource, Clone)]
pub struct RenderOutHud {
    begin: RenderOutFn,
    end: RenderOutFn,
}

impl RenderOutHud {
    /// A hook pair: `begin` lays the UI out for the spec and fills the overlay's draw list;
    /// `end` restores the window layout.
    #[must_use]
    pub fn new(begin: RenderOutFn, end: RenderOutFn) -> Self {
        Self { begin, end }
    }

    /// Prepares the HUD for `spec`; call before drawing the overlay layers.
    pub fn begin(&self, world: &mut World, spec: &RenderOutSpec) {
        (self.begin)(world, spec);
    }

    /// Restores the window layout; call after the capture.
    pub fn end(&self, world: &mut World, spec: &RenderOutSpec) {
        (self.end)(world, spec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_names_parse() {
        assert_eq!(HudLayout::parse("auto"), Some(HudLayout::Auto));
        assert_eq!(HudLayout::parse("9:16"), Some(HudLayout::Portrait));
        assert_eq!(HudLayout::parse("landscape"), Some(HudLayout::Landscape));
        assert_eq!(HudLayout::parse("square"), None);
    }

    #[test]
    fn supersampling_scales_the_render_size_only() {
        let spec = RenderOutSpec::new(1080, 1920, HudLayout::Portrait).supersampled(2);
        assert_eq!((spec.width, spec.height, spec.supersample), (2160, 3840, 2));
        assert_eq!(spec.layout, HudLayout::Portrait);
        assert_eq!(
            RenderOutSpec::new(8, 8, HudLayout::Auto)
                .supersampled(0)
                .supersample,
            1
        );
    }
}
