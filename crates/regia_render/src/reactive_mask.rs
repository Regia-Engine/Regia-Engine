// SPDX-License-Identifier: MPL-2.0
//! The temporal upscalers' reactive input: which sources write it, and where the translucent
//! layers draw relative to it (ADR-3101).
//!
//! TAAU, TSR, the FSR 3.1 port, the vendor upscalers and frame generation read one reactive mask
//! (red, 0 to 1, at the render size) through the upscaler seam. Today one source writes it: the
//! order-independent transparency composite (ADR-2065), the covered share of each pixel capped
//! at [`crate::oit::REACTIVE_SCALE`].
//!
//! Translucent layers (`regia_vfx` particles, trails and decals, through
//! [`crate::TranslucentLayer`]) need no mask for the upscalers: the frame draws them after the
//! temporal resolve, into a copy of the resolve's output-sized result, and the resolve itself
//! stays next frame's history. A particle never enters any upscaler's input or history, so it
//! leaves no trail there to remove, and a mask would only lower the history weight of the
//! background under it. `regia_vfx`'s `reactive_upscaler` test holds this for TSR, TAAU and
//! FSR 3.1 at 0.67. The consumer that does see the layers without a motion vector is frame
//! generation, which interpolates the presented image; ADR-3101 records that design and why it
//! is not built yet.
//!
//! [`ReactiveSources::of`] reports the sources for `render.get_upscalers`.

use crate::FrameRenderer;

/// A pass that writes the upscalers' reactive mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReactiveSource {
    /// The order-independent transparency composite (ADR-2065).
    Oit,
}

impl ReactiveSource {
    /// Every source, in report order.
    pub const ALL: [Self; 1] = [Self::Oit];

    /// The stable name agents see.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Oit => "oit",
        }
    }
}

/// Where the frame draws its translucent layers relative to the temporal resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LayerPlacement {
    /// After the resolve, into a copy of its output (or over the display image): the layers are
    /// in no upscaler's input or history, so they write no reactive mask.
    AfterResolve,
}

impl LayerPlacement {
    /// The stable name agents see.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::AfterResolve => "after_resolve",
        }
    }
}

/// Where this renderer draws its translucent layers (always after the resolve, ADR-041, ADR-3101).
pub const LAYER_PLACEMENT: LayerPlacement = LayerPlacement::AfterResolve;

/// The passes that read the reactive mask when the frame has one.
pub const READERS: [&str; 5] = ["taau", "tsr", "fsr3", "vendor", "frame_gen"];

/// What fed the reactive mask on the renderer's last main-view frame (`render.get_upscalers`
/// field `reactive_mask`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactiveSources {
    /// The sources that wrote the mask, in [`ReactiveSource::ALL`] order; empty when the frame
    /// had none (the upscaler then runs without one).
    pub sources: Vec<ReactiveSource>,
    /// Translucent layers installed on the renderer.
    pub layers_installed: usize,
    /// Where those layers draw ([`LAYER_PLACEMENT`]).
    pub layer_placement: LayerPlacement,
}

impl ReactiveSources {
    /// The sources of `renderer`'s last main-view frame. Reads state only; records nothing.
    #[must_use]
    pub fn of(renderer: &FrameRenderer) -> Self {
        let oit = renderer.oit_stats().reactive_mask && renderer.oit_reactive_mask().is_some();
        Self::from_parts(oit, renderer.translucent_layers().len())
    }

    /// The report from its parts: whether the OIT composite wrote the mask, and how many layers
    /// are installed.
    #[must_use]
    pub fn from_parts(oit: bool, layers_installed: usize) -> Self {
        let sources = ReactiveSource::ALL
            .into_iter()
            .filter(|source| match source {
                ReactiveSource::Oit => oit,
            })
            .collect();
        Self {
            sources,
            layers_installed,
            layer_placement: LAYER_PLACEMENT,
        }
    }

    /// `true` when the last frame handed its upscaler a reactive mask.
    #[must_use]
    pub fn has_mask(&self) -> bool {
        !self.sources.is_empty()
    }

    /// The source names, in order.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.sources.iter().map(|source| source.name()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_renderer_has_no_source_and_no_layer() {
        let report = ReactiveSources::of(&FrameRenderer::new());
        assert!(report.sources.is_empty());
        assert!(!report.has_mask());
        assert_eq!(report.layers_installed, 0);
        assert_eq!(report.layer_placement, LayerPlacement::AfterResolve);
    }

    #[test]
    fn the_oit_composite_is_the_one_source() {
        let report = ReactiveSources::from_parts(true, 2);
        assert_eq!(report.names(), vec!["oit"]);
        assert!(report.has_mask());
        assert_eq!(report.layers_installed, 2);
        // Layers alone never feed the upscalers' mask: they draw after the resolve.
        let layers_only = ReactiveSources::from_parts(false, 3);
        assert!(layers_only.names().is_empty());
        assert_eq!(layers_only.layer_placement.name(), "after_resolve");
    }

    #[test]
    fn names_are_stable_protocol_strings() {
        for source in ReactiveSource::ALL {
            let name = source.name();
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit())
            );
        }
        assert!(READERS.contains(&"frame_gen"));
    }
}
