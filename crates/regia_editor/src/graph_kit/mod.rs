// SPDX-License-Identifier: MPL-2.0
//! The graph toolkit (spec 19 § 10.10, P-186): the pieces every node-graph panel shares, so the
//! material graph, procedural graph and logic graph canvases look and behave alike.
//!
//! - [`search`]: the fuzzy node search: the scorer, ranking with a dragged-pin filter, the
//!   search list a canvas menu shows and the popup the canvas opens on Space or Tab.
//! - [`frames`]: comment frames: their grips, what a drag of each does, and the nodes a frame
//!   carries when it moves.
//! - [`reroute`]: reroute points: the curve a rerouted wire takes through them.
//! - [`badges`]: error and warning badges on a node, colored by theme tokens and spoken as their
//!   message.
//! - [`preview`]: the node preview slot (a swatch, or why there is none).
//! - [`canvas`]: the `canvas_keys` hookup: a canvas shown with the Nodes list's Enter framing and
//!   its accessible name, and the Nodes list focus that follows its node when rows move.
//!
//! Each panel keeps its own document model and edits; the toolkit draws and measures.

pub mod badges;
pub mod canvas;
pub mod frames;
pub mod preview;
pub mod reroute;
pub mod search;

#[cfg(test)]
mod tests;
