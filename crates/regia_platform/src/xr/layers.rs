// SPDX-License-Identifier: MPL-2.0
//! What a frame hands the runtime's compositor (spec 58 §§ 2.6, 2.10): one projection layer with
//! each eye's rendered pose, sub-image and optional depth information, quad layers for panels, and
//! the visibility mask of the lens.
//!
//! Plain data, so a backend needs no graphics dependency here. A projection layer carries the pose
//! each eye was *rendered* with, not the late-latched one: the runtime reprojects from the pose
//! rendered (OpenXR 1.1, "Composition", `XrCompositionLayerProjectionView`).

use super::{Fov, Pose, XrError};

/// Quad layers a frame may submit beside the projection layer when the runtime reports no lower
/// limit. OpenXR guarantees 16 composition layers per system, one of which is the projection.
pub const MAX_QUAD_LAYERS: usize = 15;

/// A rectangle of one layer of an image: `array_index` is the image's array layer (0 for a
/// single-layer target, the eye for a two-layer target), `rect` is x, y, width, height in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubImage {
    /// The array layer.
    pub array_index: u32,
    /// x, y, width, height, pixels.
    pub rect: [u32; 4],
}

/// How a depth image maps to view distance (the depth-info layer extension).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthInfo {
    /// The depth image's sub-image.
    pub image: SubImage,
    /// Smallest depth value in the image.
    pub min_depth: f32,
    /// Largest depth value in the image.
    pub max_depth: f32,
    /// The view distance `min_depth` stands for, meters. Positive; either distance may be larger
    /// (reversed-Z maps the far plane to `min_depth`) and one may be infinite.
    pub near_m: f32,
    /// The view distance `max_depth` stands for, meters.
    pub far_m: f32,
}

/// One eye of a projection layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProjectionView {
    /// The pose this eye was rendered with, in tracking space.
    pub pose: Pose,
    /// The field of view it was rendered with.
    pub fov: Fov,
    /// Where its colour image is.
    pub image: SubImage,
    /// Its depth image, when depth submission is on.
    pub depth: Option<DepthInfo>,
}

/// The scene: both eyes' images.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProjectionLayer {
    /// Left, right.
    pub views: [ProjectionView; 2],
}

/// What a quad layer is locked to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum LayerSpace {
    /// Follows the head.
    Head,
    /// Follows the rig (the tracking origin).
    #[default]
    Rig,
    /// Stays in the world.
    World,
}

impl LayerSpace {
    /// Every space, in order.
    pub const ALL: [LayerSpace; 3] = [LayerSpace::Head, LayerSpace::Rig, LayerSpace::World];

    /// The lowercase name the agent protocol uses.
    pub fn name(self) -> &'static str {
        match self {
            LayerSpace::Head => "head",
            LayerSpace::Rig => "rig",
            LayerSpace::World => "world",
        }
    }

    /// The space named `name`.
    pub fn from_name(name: &str) -> Option<LayerSpace> {
        LayerSpace::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// A flat panel the compositor draws over the scene, sharp and without post-processing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuadLayer {
    /// The panel's id.
    pub id: u32,
    /// Higher draws later (on top); equal priorities keep the panels' id order.
    pub priority: i32,
    /// What the panel is locked to.
    pub space: LayerSpace,
    /// The panel's center and orientation in `space` (it faces +Z of this pose).
    pub pose: Pose,
    /// Width and height, meters.
    pub size_m: [f32; 2],
    /// Where its image is.
    pub image: SubImage,
}

/// The layers of one frame. An empty list clears the display: a frame that did not render.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayerList {
    /// The scene, absent on a frame that rendered nothing.
    pub projection: Option<ProjectionLayer>,
    /// The panels, in draw order (see [`order_quads`]).
    pub quads: Vec<QuadLayer>,
}

impl LayerList {
    /// The layers the frame submits.
    pub fn len(&self) -> usize {
        usize::from(self.projection.is_some()) + self.quads.len()
    }

    /// Whether no layer is submitted.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Checks the list against what a runtime accepts: at most `max_layers` layers, finite poses,
    /// valid fields of view, non-empty rectangles, positive panel sizes and a depth range that
    /// runs forward. An offending layer is [`XrError::InvalidValue`] naming it.
    pub fn validate(&self, max_layers: usize) -> Result<(), XrError> {
        let bad = |key: String, reason: &str| XrError::InvalidValue {
            key,
            reason: reason.to_string(),
        };
        if self.len() > max_layers {
            return Err(bad("layers".into(), &format!("at most {max_layers} per frame")));
        }
        let rect_ok = |image: &SubImage| image.rect[2] > 0 && image.rect[3] > 0;
        if let Some(projection) = &self.projection {
            for (eye, view) in projection.views.iter().enumerate() {
                let key = || format!("projection view {eye}");
                if !finite_pose(&view.pose) {
                    return Err(bad(key(), "the pose is not finite"));
                }
                view.fov.validate()?;
                if !rect_ok(&view.image) {
                    return Err(bad(key(), "an empty image rectangle"));
                }
                if let Some(depth) = &view.depth {
                    let ordered = depth.min_depth >= 0.0
                        && depth.max_depth <= 1.0
                        && depth.min_depth < depth.max_depth
                        && depth.near_m > 0.0
                        && depth.far_m > 0.0
                        && depth.near_m != depth.far_m;
                    if !ordered || !rect_ok(&depth.image) {
                        return Err(bad(key(), "a bad depth range or depth image"));
                    }
                }
            }
        }
        for quad in &self.quads {
            let key = || format!("quad {}", quad.id);
            if !finite_pose(&quad.pose) {
                return Err(bad(key(), "the pose is not finite"));
            }
            let positive = |v: f32| v.is_finite() && v > 0.0;
            if !positive(quad.size_m[0]) || !positive(quad.size_m[1]) {
                return Err(bad(key(), "the size must be positive"));
            }
            if !rect_ok(&quad.image) {
                return Err(bad(key(), "an empty image rectangle"));
            }
        }
        Ok(())
    }
}

fn finite_pose(pose: &Pose) -> bool {
    pose.position.is_finite() && pose.orientation.is_finite()
}

/// Sorts panels into draw order: ascending priority, then id, so the order never depends on how
/// the caller stored them (OpenXR leaves equal-priority order unspecified; spec 58 § 2.10).
pub fn order_quads(quads: &mut [QuadLayer]) {
    quads.sort_by_key(|quad| (quad.priority, quad.id));
}

/// Splits panels in draw order into those the runtime composes and those over its layer budget,
/// which the engine draws into the eye images instead (spec 58 § 2.10). The projection layer takes
/// one of `max_layers`. The budget keeps the *top* panels as layers, so what overflows lies under
/// them, and an overflow panel is never drawn over a composed one.
pub fn split_quads(quads: &[QuadLayer], max_layers: usize) -> (&[QuadLayer], &[QuadLayer]) {
    let budget = max_layers.saturating_sub(1);
    let overflow = quads.len().saturating_sub(budget);
    let (under, top) = quads.split_at(overflow);
    (top, under)
}

/// The part of the display a lens never shows (the visibility mask extension, hidden triangle
/// mesh): a triangle list on the view plane `z = -1`, so `[x, y]` are tangents of the view angle
/// and the eye's projection places them with no other input.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VisibilityMask {
    /// Vertices on the plane `z = -1`.
    pub vertices: Vec<[f32; 2]>,
    /// Triangle list.
    pub indices: Vec<u32>,
}

impl VisibilityMask {
    /// The simulator's lens: everything outside the ellipse inscribed in `fov`, shrunk by
    /// `radius` (1 touches the frustum's sides, smaller hides more). `None` for a `radius`
    /// outside `0.1..=2.0` or fewer than 8 segments: the deterministic stand-in for what a runtime
    /// reports for its optics.
    pub fn lens(fov: Fov, radius: f32, segments: u32) -> Option<VisibilityMask> {
        if !(0.1..=2.0).contains(&radius) || segments < 8 {
            return None;
        }
        let (left, right) = (fov.left.tan(), fov.right.tan());
        let (down, up) = (fov.down.tan(), fov.up.tan());
        let center = [(left + right) * 0.5, (down + up) * 0.5];
        let half = [(right - left) * 0.5, (up - down) * 0.5];
        // The outer ring passes well outside the frustum's corners; rasterization clips it.
        let outer = radius.max(1.0) * 3.0;
        let mut vertices = Vec::with_capacity(segments as usize * 2);
        for i in 0..segments {
            let angle = std::f32::consts::TAU * i as f32 / segments as f32;
            let (s, c) = angle.sin_cos();
            for scale in [radius, outer] {
                vertices.push([center[0] + half[0] * c * scale, center[1] + half[1] * s * scale]);
            }
        }
        let mut indices = Vec::with_capacity(segments as usize * 6);
        for i in 0..segments {
            let next = (i + 1) % segments;
            let (a, b, c, d) = (i * 2, i * 2 + 1, next * 2, next * 2 + 1);
            indices.extend_from_slice(&[a, b, c, c, b, d]);
        }
        Some(VisibilityMask { vertices, indices })
    }

    /// Triangles in the mesh.
    pub fn triangles(&self) -> usize {
        self.indices.len() / 3
    }

    /// Whether the mesh is well formed: every index in range, a whole number of triangles and
    /// finite vertices. A runtime's mesh is checked once before it reaches the GPU.
    pub fn is_valid(&self) -> bool {
        self.indices.len().is_multiple_of(3)
            && self.indices.iter().all(|&i| (i as usize) < self.vertices.len())
            && self.vertices.iter().flatten().all(|v| v.is_finite())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::glam::{Quat, Vec3};

    fn sub(w: u32, h: u32) -> SubImage {
        SubImage {
            array_index: 0,
            rect: [0, 0, w, h],
        }
    }

    fn projection() -> ProjectionLayer {
        let view = ProjectionView {
            pose: Pose::IDENTITY,
            fov: Fov::symmetric(0.8, 0.8),
            image: sub(16, 16),
            depth: None,
        };
        ProjectionLayer { views: [view, view] }
    }

    fn quad(id: u32, priority: i32) -> QuadLayer {
        QuadLayer {
            id,
            priority,
            space: LayerSpace::Rig,
            pose: Pose::IDENTITY,
            size_m: [1.0, 0.5],
            image: sub(8, 4),
        }
    }

    fn reason(list: &LayerList) -> String {
        list.validate(16).expect_err("rejected").to_string()
    }

    #[test]
    fn a_valid_list_passes_and_an_empty_list_is_a_cleared_display() {
        assert!(LayerList::default().is_empty());
        LayerList::default().validate(16).expect("empty is legal");
        let list = LayerList {
            projection: Some(projection()),
            quads: vec![quad(1, 0)],
        };
        assert_eq!(list.len(), 2);
        list.validate(16).expect("valid");
        assert!(list.validate(1).is_err(), "over the budget");
    }

    #[test]
    fn each_bad_layer_is_named() {
        let mut bad_pose = LayerList {
            projection: Some(projection()),
            ..LayerList::default()
        };
        if let Some(p) = bad_pose.projection.as_mut() {
            p.views[1].pose.position = Vec3::NAN;
        }
        assert!(reason(&bad_pose).contains("projection view 1"));

        let mut bad_rect = LayerList {
            projection: Some(projection()),
            ..LayerList::default()
        };
        if let Some(p) = bad_rect.projection.as_mut() {
            p.views[0].image.rect[2] = 0;
        }
        assert!(reason(&bad_rect).contains("projection view 0"));

        let mut bad_depth = LayerList {
            projection: Some(projection()),
            ..LayerList::default()
        };
        if let Some(p) = bad_depth.projection.as_mut() {
            p.views[0].depth = Some(DepthInfo {
                image: sub(16, 16),
                min_depth: 1.0,
                max_depth: 0.0,
                near_m: 0.05,
                far_m: 100.0,
            });
        }
        assert!(reason(&bad_depth).contains("depth"));

        // A reversed-Z submit: `min_depth` is the far plane (infinite), `max_depth` the near one.
        let mut reversed = LayerList {
            projection: Some(projection()),
            ..LayerList::default()
        };
        if let Some(p) = reversed.projection.as_mut() {
            p.views[0].depth = Some(DepthInfo {
                image: sub(16, 16),
                min_depth: 0.0,
                max_depth: 1.0,
                near_m: f32::INFINITY,
                far_m: 0.05,
            });
        }
        reversed
            .validate(16)
            .expect("reversed-Z with an infinite far plane is legal");

        let mut small = quad(7, 0);
        small.size_m[1] = 0.0;
        let list = LayerList {
            quads: vec![small],
            ..LayerList::default()
        };
        assert!(reason(&list).contains("quad 7"));
        let mut turned = quad(8, 0);
        turned.pose.orientation = Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0);
        let list = LayerList {
            quads: vec![turned],
            ..LayerList::default()
        };
        assert!(reason(&list).contains("quad 8"));
    }

    #[test]
    fn quads_order_by_priority_then_id_whatever_the_input_order() {
        let mut quads = vec![quad(3, 1), quad(2, 0), quad(1, 1), quad(4, -2)];
        order_quads(&mut quads);
        let ids: Vec<u32> = quads.iter().map(|q| q.id).collect();
        assert_eq!(ids, [4, 2, 1, 3]);
    }

    #[test]
    fn the_budget_keeps_the_top_panels_as_layers() {
        let quads = [quad(1, 0), quad(2, 1), quad(3, 2), quad(4, 3)];
        let (composed, overflow) = split_quads(&quads, 3);
        assert_eq!(composed.iter().map(|q| q.id).collect::<Vec<_>>(), [3, 4]);
        assert_eq!(overflow.iter().map(|q| q.id).collect::<Vec<_>>(), [1, 2]);
        let (composed, overflow) = split_quads(&quads, 16);
        assert_eq!((composed.len(), overflow.len()), (4, 0));
        let (composed, overflow) = split_quads(&quads, 0);
        assert_eq!((composed.len(), overflow.len()), (0, 4));
    }

    #[test]
    fn the_lens_mask_covers_the_corners_and_leaves_the_center() {
        let fov = Fov {
            left: -0.9,
            right: 0.7,
            up: 0.8,
            down: -0.8,
        };
        // An odd count: no ring edge runs exactly through a corner, where the point-in-triangle
        // test below would read rounding noise.
        let mask = VisibilityMask::lens(fov, 0.9, 33).expect("mask");
        assert!(mask.is_valid());
        assert_eq!(mask.triangles(), 66);
        let contains = |p: [f32; 2]| {
            mask.indices.as_chunks::<3>().0.iter().any(|t| {
                let [a, b, c] = t.map(|i| mask.vertices[i as usize]);
                let side =
                    |u: [f32; 2], v: [f32; 2]| (v[0] - u[0]) * (p[1] - u[1]) - (v[1] - u[1]) * (p[0] - u[0]);
                let (s0, s1, s2) = (side(a, b), side(b, c), side(c, a));
                (s0 >= 0.0 && s1 >= 0.0 && s2 >= 0.0) || (s0 <= 0.0 && s1 <= 0.0 && s2 <= 0.0)
            })
        };
        let (l, r) = (fov.left.tan(), fov.right.tan());
        let (d, u) = (fov.down.tan(), fov.up.tan());
        for corner in [[l, d], [l, u], [r, d], [r, u]] {
            assert!(contains(corner), "corner {corner:?} is hidden");
        }
        assert!(
            !contains([(l + r) * 0.5, (d + u) * 0.5]),
            "the center stays visible"
        );
        assert!(VisibilityMask::lens(fov, 0.0, 32).is_none());
        assert!(VisibilityMask::lens(fov, 0.9, 3).is_none());
    }

    #[test]
    fn a_malformed_runtime_mask_is_detected() {
        let bad_index = VisibilityMask {
            vertices: vec![[0.0, 0.0]],
            indices: vec![0, 0, 1],
        };
        assert!(!bad_index.is_valid());
        let partial = VisibilityMask {
            vertices: vec![[0.0, 0.0]],
            indices: vec![0, 0],
        };
        assert!(!partial.is_valid());
        let nan = VisibilityMask {
            vertices: vec![[f32::NAN, 0.0]],
            indices: vec![0, 0, 0],
        };
        assert!(!nan.is_valid());
    }
}
