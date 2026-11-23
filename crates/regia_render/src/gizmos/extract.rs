// SPDX-License-Identifier: MPL-2.0
//! The renderer's per-frame gizmo input: line segments and strip joints per config batch.
//!
//! Bevy uploads line strips as one vertex stream with NaN separators and lets the GPU drop the
//! NaN segments. Regia Engine splits strips on the CPU instead: every segment and joint instance is
//! finite, so no driver (Android included) has to rasterise or cull NaN positions.
//!
//! Regia Engine-original, not ported: Bevy has no counterpart of this CPU splitting step. Only the
//! segment/joint instance layout follows the ported line pipeline (`shaders/gizmos.wgsl`).

use std::sync::Arc;

use regia_core::math::{Color, Mat4, Vec3};

use super::buffer::GizmoBufferView;
use super::config::{GizmoConfig, GizmoLineConfig, GizmoLineJoint};

/// One line segment instance (positions in the batch's local space, linear colours).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GizmoSegment {
    /// Start point.
    pub a: [f32; 3],
    /// End point.
    pub b: [f32; 3],
    /// Colour at the start.
    pub color_a: [f32; 4],
    /// Colour at the end.
    pub color_b: [f32; 4],
}

/// One line-strip joint instance: the corner at `b` between segments `a→b` and `b→c`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GizmoJoint {
    /// Previous point.
    pub a: [f32; 3],
    /// The corner.
    pub b: [f32; 3],
    /// Next point.
    pub c: [f32; 3],
    /// Colour of the corner.
    pub color: [f32; 4],
}

/// Gizmo lines that share one configuration and transform.
#[derive(Clone, Debug, PartialEq)]
pub struct GizmoBatch {
    /// Local → world (identity for immediate-mode gizmos).
    pub world_from_local: Mat4,
    /// Line settings.
    pub line: GizmoLineConfig,
    /// Depth bias in `-1..=1`.
    pub depth_bias: f32,
    /// Ignore scene depth.
    pub on_top: bool,
    /// Line segments. Shared so a retained gizmo keeps one expanded copy across frames.
    pub segments: Arc<[GizmoSegment]>,
    /// Strip joints (empty unless [`GizmoLineConfig::joints`] is set). Shared with
    /// [`Self::segments`].
    pub joints: Arc<[GizmoJoint]>,
}

impl GizmoBatch {
    /// Builds a batch from buffer data; non-finite points end a strip and are never emitted.
    #[must_use]
    pub fn from_view(view: GizmoBufferView<'_>, world_from_local: Mat4, config: &GizmoConfig) -> Self {
        let mut segments = Vec::with_capacity(view.list_positions.len() / 2 + view.strip_positions.len());
        let mut joints = Vec::new();
        let list_len = view.list_positions.len().min(view.list_colors.len());
        for i in (0..list_len / 2).map(|i| i * 2) {
            let (a, b) = (view.list_positions[i], view.list_positions[i + 1]);
            if a.is_finite() && b.is_finite() {
                segments.push(segment(a, b, view.list_colors[i], view.list_colors[i + 1]));
            }
        }
        let want_joints = config.line.joints != GizmoLineJoint::None;
        let strip_len = view.strip_positions.len().min(view.strip_colors.len());
        let positions = &view.strip_positions[..strip_len];
        let colors = &view.strip_colors[..strip_len];
        let mut run_start = 0;
        for end in 0..=strip_len {
            if end < strip_len && positions[end].is_finite() {
                continue;
            }
            let run = run_start..end;
            for i in run.clone().skip(1) {
                segments.push(segment(positions[i - 1], positions[i], colors[i - 1], colors[i]));
            }
            if want_joints {
                for i in run.skip(2) {
                    joints.push(GizmoJoint {
                        a: positions[i - 2].to_array(),
                        b: positions[i - 1].to_array(),
                        c: positions[i].to_array(),
                        color: colors[i - 1].to_array(),
                    });
                }
            }
            run_start = end + 1;
        }
        Self {
            world_from_local,
            line: config.line.clone(),
            depth_bias: config.depth_bias,
            on_top: config.on_top,
            segments: Arc::from(segments),
            joints: Arc::from(joints),
        }
    }

    /// `true` when the batch draws nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
}

fn segment(a: Vec3, b: Vec3, color_a: Color, color_b: Color) -> GizmoSegment {
    GizmoSegment {
        a: a.to_array(),
        b: b.to_array(),
        color_a: color_a.to_array(),
        color_b: color_b.to_array(),
    }
}

/// One corner of a debug-label glyph quad (world space, linear colour): six per glyph, two
/// triangles (ADR-7254).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GizmoLabelVertex {
    /// World position.
    pub position: [f32; 3],
    /// Texel coordinate on the batch's atlas page (V down).
    pub uv: [f32; 2],
    /// Linear RGBA; the page's coverage multiplies the alpha.
    pub color: [f32; 4],
}

/// Debug-label glyph quads that sample one atlas page ([`crate::DebugLabelGlyphs`]).
#[derive(Clone, Debug, PartialEq)]
pub struct GizmoLabelBatch {
    /// Ignore scene depth.
    pub on_top: bool,
    /// The atlas page the quads sample (index, texels).
    pub page: (u32, crate::DebugLabelPage),
    /// Six vertices per glyph.
    pub vertices: Arc<[GizmoLabelVertex]>,
}

/// Everything the gizmo pass draws in one frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GizmoFrame {
    /// Batches in draw order.
    pub batches: Vec<GizmoBatch>,
    /// Debug labels drawn from a glyph atlas, after the line batches (empty without a
    /// [`crate::DebugLabelGlyphSource`]; the stroke-font labels are line batches).
    pub labels: Vec<GizmoLabelBatch>,
}

impl GizmoFrame {
    /// `true` when nothing will be drawn.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.batches.iter().all(GizmoBatch::is_empty) && self.labels.iter().all(|l| l.vertices.is_empty())
    }

    /// Total line segments.
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.batches.iter().map(|b| b.segments.len()).sum()
    }

    /// Total strip joints.
    #[must_use]
    pub fn joint_count(&self) -> usize {
        self.batches.iter().map(|b| b.joints.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gizmos::{DefaultGizmoConfigGroup, GizmoBuffer};

    #[test]
    fn strips_split_into_segments_without_crossing_separators() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.linestrip([Vec3::ZERO, Vec3::X, Vec3::Y], Color::RED);
        b.linestrip([Vec3::Z, Vec3::ONE], Color::BLUE);
        b.line(Vec3::ZERO, Vec3::NEG_X, Color::GREEN);
        let batch = GizmoBatch::from_view(b.buffer(), Mat4::IDENTITY, &GizmoConfig::default());
        assert_eq!(batch.segments.len(), 1 + 2 + 1);
        assert!(batch.joints.is_empty(), "no joints unless configured");
        assert!(
            batch
                .segments
                .iter()
                .all(|s| s.a.iter().chain(&s.b).all(|v| v.is_finite()))
        );
        // The second strip keeps its colour; the strips do not join each other.
        assert!(
            batch
                .segments
                .iter()
                .any(|s| s.a == Vec3::Z.to_array() && s.color_a == Color::BLUE.to_array())
        );
        assert!(!batch.segments.iter().any(|s| s.a == Vec3::Y.to_array()));
    }

    #[test]
    fn joints_come_from_interior_strip_points() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.lineloop([Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z], Color::WHITE);
        let config = GizmoConfig {
            line: GizmoLineConfig {
                joints: GizmoLineJoint::Miter,
                ..GizmoLineConfig::default()
            },
            ..GizmoConfig::default()
        };
        let batch = GizmoBatch::from_view(b.buffer(), Mat4::IDENTITY, &config);
        // A closed loop of four points is drawn as six points: five segments, four joints.
        assert_eq!(batch.segments.len(), 5);
        assert_eq!(batch.joints.len(), 4);
        let corners: Vec<_> = batch.joints.iter().map(|j| j.b).collect();
        for p in [Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z] {
            assert!(
                corners.contains(&p.to_array()),
                "every corner of the loop has a joint"
            );
        }
    }

    #[test]
    fn mismatched_or_odd_lists_never_index_out_of_range() {
        let view = GizmoBufferView {
            list_positions: &[Vec3::ZERO, Vec3::X, Vec3::Y],
            list_colors: &[Color::WHITE; 2],
            strip_positions: &[Vec3::ZERO, Vec3::X, Vec3::Y],
            strip_colors: &[Color::WHITE; 2],
        };
        let batch = GizmoBatch::from_view(view, Mat4::IDENTITY, &GizmoConfig::default());
        assert_eq!(batch.segments.len(), 2);
    }
}
