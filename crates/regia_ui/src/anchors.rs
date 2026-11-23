// SPDX-License-Identifier: MPL-2.0
//! Anchor presets (ADR-273 Y01, ADR-1842): one name sets the inset, size and pivot of a node that
//! sits at an edge, corner or the centre of its parent, the way a designer thinks about HUD
//! placement.
//!
//! A preset turns an out-of-flow node (`position: absolute`) into `inset` values the layout
//! already understands, so nothing in the layout engine changes and the result is correct at
//! every resolution and UI scale. The `offsets` are distances from the parent's edges in UI
//! pixels, in the order left, top, right, bottom. On an axis the node is centred on, the offset
//! shifts the node (`left - right`, `top - bottom`). Right-to-left layout mirrors `inset` as for
//! any other node (ADR-518).
//!
//! The agent surface is `ui.set_anchor_preset` (`book/agents/ui-anchors.md`).

use crate::style::{Dimension, Edges, PositionType, Style};

/// Where a node is anchored inside its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnchorPreset {
    /// Top-left corner.
    TopLeft,
    /// Middle of the top edge.
    TopCenter,
    /// Top-right corner.
    TopRight,
    /// Middle of the left edge.
    CenterLeft,
    /// Centre of the parent.
    Center,
    /// Middle of the right edge.
    CenterRight,
    /// Bottom-left corner.
    BottomLeft,
    /// Middle of the bottom edge.
    BottomCenter,
    /// Bottom-right corner.
    BottomRight,
    /// Spans the top edge.
    TopWide,
    /// Spans the bottom edge.
    BottomWide,
    /// Spans the left edge.
    LeftWide,
    /// Spans the right edge.
    RightWide,
    /// Spans the width, centred vertically.
    VCenterWide,
    /// Spans the height, centred horizontally.
    HCenterWide,
    /// Fills the parent.
    FullRect,
}

/// Why a preset could not be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorError {
    /// A centred axis needs a fixed (`Points` or `Percent`) size on that axis.
    SizeRequired,
    /// A preserved rectangle or its containing size is not finite or has negative extent.
    InvalidGeometry,
}

impl AnchorPreset {
    /// Every preset, in documentation order.
    pub const ALL: [AnchorPreset; 16] = [
        Self::TopLeft,
        Self::TopCenter,
        Self::TopRight,
        Self::CenterLeft,
        Self::Center,
        Self::CenterRight,
        Self::BottomLeft,
        Self::BottomCenter,
        Self::BottomRight,
        Self::TopWide,
        Self::BottomWide,
        Self::LeftWide,
        Self::RightWide,
        Self::VCenterWide,
        Self::HCenterWide,
        Self::FullRect,
    ];

    /// The stable snake_case name used by the agent protocol and `.ui.ron` files.
    pub const fn name(self) -> &'static str {
        match self {
            Self::TopLeft => "top_left",
            Self::TopCenter => "top_center",
            Self::TopRight => "top_right",
            Self::CenterLeft => "center_left",
            Self::Center => "center",
            Self::CenterRight => "center_right",
            Self::BottomLeft => "bottom_left",
            Self::BottomCenter => "bottom_center",
            Self::BottomRight => "bottom_right",
            Self::TopWide => "top_wide",
            Self::BottomWide => "bottom_wide",
            Self::LeftWide => "left_wide",
            Self::RightWide => "right_wide",
            Self::VCenterWide => "vcenter_wide",
            Self::HCenterWide => "hcenter_wide",
            Self::FullRect => "full_rect",
        }
    }

    /// The preset called `name`, if any.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }

    /// Horizontal and vertical behavior: `-1` pinned to the start edge, `0` centred, `1` pinned
    /// to the end edge, `2` stretched between both edges.
    const fn axes(self) -> (i8, i8) {
        match self {
            Self::TopLeft => (-1, -1),
            Self::TopCenter => (0, -1),
            Self::TopRight => (1, -1),
            Self::CenterLeft => (-1, 0),
            Self::Center => (0, 0),
            Self::CenterRight => (1, 0),
            Self::BottomLeft => (-1, 1),
            Self::BottomCenter => (0, 1),
            Self::BottomRight => (1, 1),
            Self::TopWide => (2, -1),
            Self::BottomWide => (2, 1),
            Self::LeftWide => (-1, 2),
            Self::RightWide => (1, 2),
            Self::VCenterWide => (2, 0),
            Self::HCenterWide => (0, 2),
            Self::FullRect => (2, 2),
        }
    }

    /// The pivot ratio that makes scale and rotation-free transforms grow away from the anchor.
    pub const fn pivot_ratio(self) -> [f32; 2] {
        let (h, v) = self.axes();
        [pivot(h), pivot(v)]
    }

    /// Applies an anchor while preserving a measured border rectangle in UI pixels.
    /// `rect` is relative to the parent's padding-box origin; `parent_size` is that box's size.
    /// Coordinates are logical left-to-right coordinates; mirror the measured X position first
    /// for a right-to-left node. Fixed axes take the measured size, stretched axes take offsets
    /// to both edges.
    /// The operation is atomic on invalid geometry.
    pub fn apply_preserving_rect(
        self,
        style: &mut Style,
        rect: [f32; 4],
        parent_size: [f32; 2],
    ) -> Result<(), AnchorError> {
        if !rect.iter().chain(parent_size.iter()).all(|v| v.is_finite())
            || rect[2..].iter().chain(parent_size.iter()).any(|v| *v < 0.0)
        {
            return Err(AnchorError::InvalidGeometry);
        }
        let (horizontal, vertical) = self.axes();
        let offsets = |mode, at, size, parent| match mode {
            0 => [at - (parent - size) * 0.5, 0.0],
            _ => [at, parent - at - size],
        };
        let x = offsets(horizontal, rect[0], rect[2], parent_size[0]);
        let y = offsets(vertical, rect[1], rect[3], parent_size[1]);
        if !x.iter().chain(y.iter()).all(|v| v.is_finite()) {
            return Err(AnchorError::InvalidGeometry);
        }
        let mut next = style.clone();
        next.size = [Dimension::Points(rect[2]), Dimension::Points(rect[3])];
        self.apply(&mut next, [x[0], y[0], x[1], y[1]])?;
        *style = next;
        Ok(())
    }

    /// Applies the preset to `style`: sets `position`, `inset` and, for stretched axes, clears the
    /// size on that axis. `offsets` is `[left, top, right, bottom]` in UI pixels; non-finite values
    /// count as 0. On error `style` is unchanged.
    pub fn apply(self, style: &mut Style, offsets: [f32; 4]) -> Result<(), AnchorError> {
        let o = offsets.map(|v| if v.is_finite() { v } else { 0.0 });
        let (h, v) = self.axes();
        let (ix, mx, sx) = axis(h, o[0], o[2], style.size[0])?;
        let (iy, my, sy) = axis(v, o[1], o[3], style.size[1])?;
        style.position = PositionType::Absolute;
        style.inset = Edges {
            left: ix.0,
            right: ix.1,
            top: iy.0,
            bottom: iy.1,
        };
        style.margin.left = mx;
        style.margin.right = Dimension::Auto;
        style.margin.top = my;
        style.margin.bottom = Dimension::Auto;
        style.size = [sx, sy];
        Ok(())
    }
}

const fn pivot(mode: i8) -> f32 {
    match mode {
        -1 => 0.0,
        1 => 1.0,
        _ => 0.5,
    }
}

type Pair = (Dimension, Dimension);

/// `(inset start/end, start margin, size)` for one axis.
fn axis(
    mode: i8,
    start: f32,
    end: f32,
    size: Dimension,
) -> Result<(Pair, Dimension, Dimension), AnchorError> {
    let px = Dimension::Points;
    Ok(match mode {
        -1 => ((px(start), Dimension::Auto), Dimension::Auto, size),
        1 => ((Dimension::Auto, px(end)), Dimension::Auto, size),
        2 => ((px(start), px(end)), Dimension::Auto, Dimension::Auto),
        _ => {
            let shift = start - end;
            match size {
                Dimension::Points(s) => (
                    (Dimension::Percent(50.0), Dimension::Auto),
                    px(shift - s * 0.5),
                    size,
                ),
                Dimension::Percent(p) => (
                    (Dimension::Percent((100.0 - p) * 0.5), Dimension::Auto),
                    px(shift),
                    size,
                ),
                Dimension::Auto => return Err(AnchorError::SizeRequired),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use taffy::prelude::{AvailableSpace, TaffyTree, length};

    /// Lays `style` out inside a `w` x `h` parent and returns `[x, y, width, height]`.
    pub(super) fn place(style: &Style, w: f32, h: f32) -> [f32; 4] {
        let mut tree: TaffyTree<()> = TaffyTree::new();
        let child = tree.new_leaf(style.to_taffy(1.0)).unwrap();
        let root = tree
            .new_with_children(
                taffy::style::Style {
                    size: taffy::geometry::Size {
                        width: length(w),
                        height: length(h),
                    },
                    ..Default::default()
                },
                &[child],
            )
            .unwrap();
        tree.compute_layout(
            root,
            taffy::geometry::Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MaxContent,
            },
        )
        .unwrap();
        let l = tree.layout(child).unwrap();
        [l.location.x, l.location.y, l.size.width, l.size.height]
    }

    fn sized(w: f32, h: f32) -> Style {
        Style {
            size: [Dimension::Points(w), Dimension::Points(h)],
            ..Style::default()
        }
    }

    const RES: [(f32, f32); 3] = [(1280.0, 720.0), (1920.0, 1080.0), (800.0, 1280.0)];

    #[test]
    fn corners_and_edges_at_three_resolutions() {
        for (w, h) in RES {
            let cases: [(AnchorPreset, [f32; 2]); 9] = [
                (AnchorPreset::TopLeft, [10.0, 20.0]),
                (AnchorPreset::TopCenter, [(w - 100.0) / 2.0 + 10.0 - 30.0, 20.0]),
                (AnchorPreset::TopRight, [w - 100.0 - 30.0, 20.0]),
                (AnchorPreset::CenterLeft, [10.0, (h - 40.0) / 2.0 + 20.0 - 40.0]),
                (
                    AnchorPreset::Center,
                    [(w - 100.0) / 2.0 + 10.0 - 30.0, (h - 40.0) / 2.0 + 20.0 - 40.0],
                ),
                (
                    AnchorPreset::CenterRight,
                    [w - 100.0 - 30.0, (h - 40.0) / 2.0 + 20.0 - 40.0],
                ),
                (AnchorPreset::BottomLeft, [10.0, h - 40.0 - 40.0]),
                (
                    AnchorPreset::BottomCenter,
                    [(w - 100.0) / 2.0 + 10.0 - 30.0, h - 40.0 - 40.0],
                ),
                (AnchorPreset::BottomRight, [w - 100.0 - 30.0, h - 40.0 - 40.0]),
            ];
            for (preset, [x, y]) in cases {
                let mut s = sized(100.0, 40.0);
                preset.apply(&mut s, [10.0, 20.0, 30.0, 40.0]).unwrap();
                let r = place(&s, w, h);
                assert!((r[0] - x).abs() < 0.01, "{preset:?} x at {w}x{h}: {r:?} want {x}");
                assert!((r[1] - y).abs() < 0.01, "{preset:?} y at {w}x{h}: {r:?} want {y}");
                assert_eq!([r[2], r[3]], [100.0, 40.0]);
            }
        }
    }

    #[test]
    fn wide_presets_stretch() {
        for (w, h) in RES {
            let mut s = sized(100.0, 40.0);
            AnchorPreset::TopWide
                .apply(&mut s, [8.0, 4.0, 12.0, 0.0])
                .unwrap();
            assert_eq!(place(&s, w, h), [8.0, 4.0, w - 20.0, 40.0]);
            let mut s = sized(100.0, 40.0);
            AnchorPreset::FullRect
                .apply(&mut s, [1.0, 2.0, 3.0, 4.0])
                .unwrap();
            assert_eq!(place(&s, w, h), [1.0, 2.0, w - 4.0, h - 6.0]);
            let mut s = sized(100.0, 40.0);
            AnchorPreset::RightWide
                .apply(&mut s, [0.0, 5.0, 6.0, 7.0])
                .unwrap();
            assert_eq!(place(&s, w, h), [w - 106.0, 5.0, 100.0, h - 12.0]);
            let mut s = sized(100.0, 40.0);
            AnchorPreset::VCenterWide.apply(&mut s, [0.0; 4]).unwrap();
            assert_eq!(place(&s, w, h), [0.0, (h - 40.0) / 2.0, w, 40.0]);
        }
    }

    #[test]
    fn percent_size_centres_and_auto_size_is_rejected() {
        let mut s = Style {
            size: [Dimension::Percent(50.0), Dimension::Percent(50.0)],
            ..Style::default()
        };
        AnchorPreset::Center.apply(&mut s, [0.0; 4]).unwrap();
        assert_eq!(place(&s, 800.0, 600.0), [200.0, 150.0, 400.0, 300.0]);
        let mut auto = Style::default();
        let before = auto.clone();
        assert_eq!(
            AnchorPreset::Center.apply(&mut auto, [0.0; 4]),
            Err(AnchorError::SizeRequired)
        );
        assert_eq!(auto, before, "an error leaves the style unchanged");
    }

    #[test]
    fn names_round_trip_and_nan_offsets_are_zero() {
        for p in AnchorPreset::ALL {
            assert_eq!(AnchorPreset::from_name(p.name()), Some(p));
        }
        assert_eq!(AnchorPreset::from_name("nope"), None);
        let mut s = sized(10.0, 10.0);
        AnchorPreset::TopLeft
            .apply(&mut s, [f32::NAN, f32::INFINITY, 0.0, 0.0])
            .unwrap();
        assert_eq!(s.inset.left, Dimension::Points(0.0));
        assert_eq!(AnchorPreset::BottomRight.pivot_ratio(), [1.0, 1.0]);
        assert_eq!(AnchorPreset::Center.pivot_ratio(), [0.5, 0.5]);
    }
}

#[cfg(test)]
mod keep_position_tests {
    use super::*;

    #[test]
    fn every_preset_preserves_rect_at_three_sizes() {
        for parent in [[1280.0, 720.0], [1920.0, 1080.0], [800.0, 1280.0]] {
            for preset in AnchorPreset::ALL {
                let mut style = Style::default();
                let rect = [37.0, 53.0, 123.0, 71.0];
                preset.apply_preserving_rect(&mut style, rect, parent).unwrap();
                assert_eq!(
                    super::tests::place(&style, parent[0], parent[1]),
                    rect,
                    "{preset:?}"
                );
            }
        }
    }
    #[test]
    fn invalid_keep_geometry_leaves_style_unchanged() {
        let original = Style {
            size: [Dimension::Percent(40.0), Dimension::Auto],
            ..Default::default()
        };
        for rect in [
            [f32::NAN, 0.0, 1.0, 1.0],
            [0.0, 0.0, -1.0, 1.0],
            [f32::MAX, 0.0, f32::MAX, 1.0],
        ] {
            let mut style = original.clone();
            assert_eq!(
                AnchorPreset::FullRect.apply_preserving_rect(&mut style, rect, [100.0, 100.0]),
                Err(AnchorError::InvalidGeometry)
            );
            assert_eq!(style, original);
        }
    }
}
