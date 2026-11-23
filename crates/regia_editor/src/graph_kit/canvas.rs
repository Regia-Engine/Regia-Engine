// SPDX-License-Identifier: MPL-2.0
//! The `canvas_keys` hookup of a graph canvas (P-186): the canvas drawn with the Nodes list's
//! Enter framing (`canvas_keys::frame_snarl`) and its accessible name
//! (`canvas_keys::name_canvas`), and the Nodes list focus that follows its node when the rows
//! move.

use egui_snarl::ui::{SnarlViewer, SnarlWidget};
use egui_snarl::{NodeId, Snarl};

/// Shows `snarl` with `viewer` through `widget`. With `frame` set (Enter on a Nodes list row, a
/// Problems link), this frame draws that node alone, so egui-snarl's first-frame fit centers the
/// view on it (the caller gives the widget a fresh id salt for it, `GraphKeys::frame_epoch`).
/// The canvas is named `name` for screen readers. Returns the canvas response.
pub fn show<T: Clone, V: SnarlViewer<T>>(
    ui: &mut egui::Ui,
    widget: &SnarlWidget,
    snarl: &mut Snarl<T>,
    viewer: &mut V,
    frame: Option<NodeId>,
    name: &str,
) -> egui::Response {
    let response = match frame {
        Some(node) => widget.show(&mut crate::canvas_keys::frame_snarl(snarl, node), viewer, ui),
        None => widget.show(snarl, viewer, ui),
    };
    crate::canvas_keys::name_canvas(ui.ctx(), response.id, name);
    response
}

/// Moves the Nodes list's keyboard focus to `anchor`'s row in `ids` (the node it was on), so the
/// focus follows its node when rows are added, removed or reordered. Call before drawing the list.
pub fn follow_anchor<K: PartialEq>(keys: &mut crate::canvas_keys::GraphKeys, anchor: Option<&K>, ids: &[K]) {
    let row = anchor.and_then(|a| ids.iter().position(|id| id == a));
    if row.is_some() {
        keys.list.retarget(row);
    }
}

/// The node the Nodes list's focused row shows (the anchor to keep for the next frame).
#[must_use]
pub fn anchor_of<K: Clone>(keys: &crate::canvas_keys::GraphKeys, ids: &[K]) -> Option<K> {
    keys.list.current.and_then(|row| ids.get(row).cloned())
}
