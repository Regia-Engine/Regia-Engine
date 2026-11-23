// SPDX-License-Identifier: MPL-2.0
//! The node preview slot (P-186): the small swatch a node body shows of its result, or a weak
//! "no preview" whose hover text says why. The panel renders and caches the swatches; the slot
//! only shows one, at a size that follows `appearance.ui_scale` (egui points).

/// The side of a node swatch (points).
pub const NODE_SWATCH_SIDE: f32 = 48.0;

/// What a node's preview slot holds this frame.
#[derive(Clone, Copy)]
pub enum Slot<'a> {
    /// The rendered swatch.
    Ready(&'a egui::TextureHandle),
    /// No swatch, and why (a compile error, an unsupported node).
    Failed(&'a str),
    /// Not rendered yet (or not wanted): the slot draws nothing.
    Empty,
}

/// Draws `slot` into a node body, the swatch `side` points square. Returns the image's or the
/// note's response (`None` for [`Slot::Empty`]).
pub fn show(ui: &mut egui::Ui, slot: Slot<'_>, side: f32) -> Option<egui::Response> {
    match slot {
        Slot::Ready(texture) => Some(ui.image((texture.id(), egui::vec2(side, side)))),
        Slot::Failed(why) => Some(ui.weak("no preview").on_hover_text(why)),
        Slot::Empty => None,
    }
}
