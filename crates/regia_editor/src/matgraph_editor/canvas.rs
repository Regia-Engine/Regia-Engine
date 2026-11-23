// SPDX-License-Identifier: MPL-2.0
//! Canvas geometry of the Material Graph annotations (ADR-9970): the curve a rerouted wire
//! takes through its points, and the grips of a comment frame (header, edges, corners) with
//! what a drag of each does to the frame. The geometry is the graph toolkit's
//! (`crate::graph_kit::{frames, reroute}`, P-186); this module keeps the material graph's names.

pub use crate::graph_kit::frames::{
    COMMENT_EDGE, COMMENT_HEADER, COMMENT_MIN_SIZE, FrameGrip, drag_frame, frame_grips,
};
pub use crate::graph_kit::reroute::{REROUTE_GRIP, REROUTE_LEAD, REROUTE_SAMPLES, reroute_path};

// The tests below name the egui geometry types through `super::*`.
#[cfg(test)]
use egui::{Pos2, pos2, vec2};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reroute_path_passes_through_every_point() {
        let from = pos2(0.0, 0.0);
        let to = pos2(400.0, 50.0);
        let points = [pos2(100.0, -120.0), pos2(250.0, -120.0), pos2(300.0, 200.0)];
        let path = reroute_path(from, &points, to);
        assert_eq!(path.len(), (points.len() + 1) * REROUTE_SAMPLES + 1);
        assert_eq!(path.first(), Some(&from));
        assert_eq!(path.last(), Some(&to));
        for (i, p) in points.iter().enumerate() {
            assert_eq!(
                path[(i + 1) * REROUTE_SAMPLES],
                *p,
                "knot {i} is a vertex, exactly"
            );
        }
        // Smooth: no sample jumps far from its neighbours.
        for pair in path.windows(2) {
            assert!((pair[1] - pair[0]).length() < 80.0, "{pair:?}");
        }
        // It leaves the output to the right and enters the input from the left.
        assert!(path[1].x > from.x);
        assert!(path[path.len() - 2].x < to.x);
    }

    #[test]
    fn a_wire_without_points_is_one_span() {
        let path = reroute_path(pos2(0.0, 0.0), &[], pos2(100.0, 0.0));
        assert_eq!(path.len(), REROUTE_SAMPLES + 1);
        // Level pins: the curve stays on the line.
        assert!(path.iter().all(|p| p.y.abs() < 1e-4), "{path:?}");
    }

    #[test]
    fn header_moves_edges_and_corners_resize_with_a_minimum() {
        let (min, max) = ([0.0, 0.0], [200.0, 100.0]);
        assert_eq!(
            drag_frame(min, max, FrameGrip::Move, vec2(40.0, 0.0)),
            ([40.0, 0.0], [240.0, 100.0])
        );
        let grips = frame_grips(min, max);
        let at = |p: Pos2| grips.iter().rev().find(|(_, r)| r.contains(p)).map(|(g, _)| *g);
        assert_eq!(at(pos2(100.0, 10.0)), Some(FrameGrip::Move), "the header");
        assert_eq!(at(pos2(100.0, 60.0)), None, "the body pans the canvas");
        let corner = at(pos2(200.0, 100.0)).expect("bottom-right corner");
        assert_eq!(
            drag_frame(min, max, corner, vec2(20.0, 40.0)),
            ([0.0, 0.0], [220.0, 140.0])
        );
        let left = at(pos2(0.0, 50.0)).expect("left edge");
        assert_eq!(left.cursor(), egui::CursorIcon::ResizeHorizontal);
        assert_eq!(
            drag_frame(min, max, left, vec2(500.0, 0.0)),
            ([200.0 - COMMENT_MIN_SIZE, 0.0], [200.0, 100.0]),
            "never narrower than the minimum"
        );
    }
}
