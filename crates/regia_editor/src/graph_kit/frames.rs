// SPDX-License-Identifier: MPL-2.0
//! Comment frames (ADR-9970; P-186): the grips of a frame (header, edges, corners), what a
//! drag of each does to the frame, and the nodes a frame carries when its header moves it.
//! Pure functions; a panel draws and drags with them.

use egui::{Rect, Vec2, pos2, vec2};

/// Height of a comment frame's header strip, the part that moves the frame (canvas points).
pub const COMMENT_HEADER: f32 = 28.0;

/// Half-width of a comment frame's edge and corner grips (canvas points).
pub const COMMENT_EDGE: f32 = 6.0;

/// The smallest a resize leaves a comment frame (canvas points, each side).
pub const COMMENT_MIN_SIZE: f32 = 40.0;

/// What part of a comment frame a drag holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameGrip {
    /// The header strip: the frame (and the nodes inside it) moves.
    Move,
    /// An edge or a corner: those sides follow the pointer.
    Resize {
        /// The left side moves.
        left: bool,
        /// The right side moves.
        right: bool,
        /// The top side moves.
        top: bool,
        /// The bottom side moves.
        bottom: bool,
    },
}

impl FrameGrip {
    /// The cursor shown over the grip.
    #[must_use]
    pub fn cursor(self) -> egui::CursorIcon {
        match self {
            Self::Move => egui::CursorIcon::Grab,
            Self::Resize {
                left,
                right,
                top,
                bottom,
            } => match (left || right, top || bottom) {
                (true, false) => egui::CursorIcon::ResizeHorizontal,
                (false, true) => egui::CursorIcon::ResizeVertical,
                _ if (left && top) || (right && bottom) => egui::CursorIcon::ResizeNwSe,
                _ => egui::CursorIcon::ResizeNeSw,
            },
        }
    }
}

/// The grips of the frame `min`..`max`, each with its rectangle (canvas points), in the order
/// the panel registers them (later ones win where they overlap): header, edges, corners.
#[must_use]
pub fn frame_grips(min: [f32; 2], max: [f32; 2]) -> Vec<(FrameGrip, Rect)> {
    let (a, b) = (pos2(min[0], min[1]), pos2(max[0], max[1]));
    let e = COMMENT_EDGE;
    let resize = |left, right, top, bottom| FrameGrip::Resize {
        left,
        right,
        top,
        bottom,
    };
    let header_bottom = (a.y + COMMENT_HEADER).min(b.y - e);
    let mut grips = vec![(
        FrameGrip::Move,
        Rect::from_min_max(pos2(a.x + e, a.y + e), pos2(b.x - e, header_bottom)),
    )];
    grips.push((
        resize(true, false, false, false),
        Rect::from_min_max(pos2(a.x - e, a.y + e), pos2(a.x + e, b.y - e)),
    ));
    grips.push((
        resize(false, true, false, false),
        Rect::from_min_max(pos2(b.x - e, a.y + e), pos2(b.x + e, b.y - e)),
    ));
    grips.push((
        resize(false, false, true, false),
        Rect::from_min_max(pos2(a.x + e, a.y - e), pos2(b.x - e, a.y + e)),
    ));
    grips.push((
        resize(false, false, false, true),
        Rect::from_min_max(pos2(a.x + e, b.y - e), pos2(b.x - e, b.y + e)),
    ));
    for (left, top) in [(true, true), (false, true), (true, false), (false, false)] {
        let corner = pos2(if left { a.x } else { b.x }, if top { a.y } else { b.y });
        grips.push((
            resize(left, !left, top, !top),
            Rect::from_center_size(corner, vec2(2.0 * e, 2.0 * e)),
        ));
    }
    grips
}

/// The frame `min`..`max` after a drag of `grip` by `delta` (canvas points); a resize never
/// leaves a side shorter than [`COMMENT_MIN_SIZE`].
#[must_use]
pub fn drag_frame(min: [f32; 2], max: [f32; 2], grip: FrameGrip, delta: Vec2) -> ([f32; 2], [f32; 2]) {
    match grip {
        FrameGrip::Move => (
            [min[0] + delta.x, min[1] + delta.y],
            [max[0] + delta.x, max[1] + delta.y],
        ),
        FrameGrip::Resize {
            left,
            right,
            top,
            bottom,
        } => {
            let (mut lo, mut hi) = (min, max);
            if left {
                lo[0] = (min[0] + delta.x).min(max[0] - COMMENT_MIN_SIZE);
            }
            if right {
                hi[0] = (max[0] + delta.x).max(min[0] + COMMENT_MIN_SIZE);
            }
            if top {
                lo[1] = (min[1] + delta.y).min(max[1] - COMMENT_MIN_SIZE);
            }
            if bottom {
                hi[1] = (max[1] + delta.y).max(min[1] + COMMENT_MIN_SIZE);
            }
            (lo, hi)
        }
    }
}

/// Whether a node at `at` (its top-left corner, canvas points) lies inside the frame
/// `min`..`max` (edges included): the nodes a frame carries when it moves.
#[must_use]
pub fn holds(min: [f32; 2], max: [f32; 2], at: [f32; 2]) -> bool {
    (min[0]..=max[0]).contains(&at[0]) && (min[1]..=max[1]).contains(&at[1])
}

/// The nodes of `positions` inside the frame `min`..`max` ([`holds`]), in their given order.
#[must_use]
pub fn nodes_inside<K>(
    min: [f32; 2],
    max: [f32; 2],
    positions: impl IntoIterator<Item = (K, [f32; 2])>,
) -> Vec<K> {
    positions
        .into_iter()
        .filter(|(_, at)| holds(min, max, *at))
        .map(|(node, _)| node)
        .collect()
}

/// The carried nodes' positions after the frame moved by `travel`: each node keeps its place
/// inside the frame.
#[must_use]
pub fn carry<K: Clone>(nodes: &[(K, [f32; 2])], travel: Vec2) -> Vec<(K, [f32; 2])> {
    nodes
        .iter()
        .map(|(node, at)| (node.clone(), [at[0] + travel.x, at[1] + travel.y]))
        .collect()
}
