// SPDX-License-Identifier: MPL-2.0
//! The camera the renderer draws from. It is plain state: the drag controller decides where it goes.

use glam::{Mat4, Vec3, Vec4};

/// A perspective camera in millimeters. The viewer's world has +X to the right, +Y up and the piece's
/// front facing +Z.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewerCamera {
    /// The eye position.
    pub eye: [f32; 3],
    /// The point looked at.
    pub target: [f32; 3],
    /// The up direction.
    pub up: [f32; 3],
    /// Field of view across the shorter side of the target, in degrees: a piece that fits stays in view
    /// in portrait and in landscape.
    pub fov_deg: f32,
    /// Near plane distance.
    pub near_mm: f32,
    /// Far plane distance.
    pub far_mm: f32,
}

impl ViewerCamera {
    /// The straight-on front view of a recipe's `camera_default`: `distance_mm` from the origin along
    /// +Z, with the field of view of `focal_length_mm` on a `sensor_mm` sensor.
    #[must_use]
    pub fn front(distance_mm: f32, focal_length_mm: f32, sensor_mm: f32) -> Self {
        let distance = distance_mm.max(0.01);
        let fov = 2.0 * (sensor_mm / (2.0 * focal_length_mm.max(0.01))).atan();
        Self {
            eye: [0.0, 0.0, distance],
            target: [0.0; 3],
            up: [0.0, 1.0, 0.0],
            fov_deg: fov.to_degrees(),
            near_mm: distance * 0.1,
            far_mm: distance * 4.0,
        }
    }

    /// World to clip space for a target of this aspect ratio (width / height).
    #[must_use]
    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        let aspect = if aspect.is_finite() && aspect > 0.0 {
            aspect
        } else {
            1.0
        };
        let fov = if self.fov_deg.is_finite() {
            self.fov_deg.clamp(0.1, 170.0)
        } else {
            20.0
        };
        let half_short = (fov.to_radians() * 0.5).tan();
        let half_vertical = if aspect >= 1.0 {
            half_short
        } else {
            half_short / aspect
        };
        let near = self.near_mm.max(1e-3);
        let far = self.far_mm.max(near * 1.001);
        // Right-handed, depth 0 at the near plane and 1 at the far plane.
        let f = 1.0 / half_vertical;
        let depth = far / (near - far);
        let proj = Mat4::from_cols(
            Vec4::new(f / aspect, 0.0, 0.0, 0.0),
            Vec4::new(0.0, f, 0.0, 0.0),
            Vec4::new(0.0, 0.0, depth, -1.0),
            Vec4::new(0.0, 0.0, near * depth, 0.0),
        );
        let eye = Vec3::from(self.eye);
        let forward = (Vec3::from(self.target) - eye).normalize_or_zero();
        let right = forward.cross(Vec3::from(self.up)).normalize_or_zero();
        let up = right.cross(forward);
        let view = Mat4::from_cols(
            Vec4::new(right.x, up.x, -forward.x, 0.0),
            Vec4::new(right.y, up.y, -forward.y, 0.0),
            Vec4::new(right.z, up.z, -forward.z, 0.0),
            Vec4::new(-right.dot(eye), -up.dot(eye), forward.dot(eye), 1.0),
        );
        proj * view
    }
}

impl Default for ViewerCamera {
    /// The default of the piece recipe format: 60 mm away, a 100 mm lens on a 36 mm sensor.
    fn default() -> Self {
        Self::front(60.0, 100.0, 36.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_front_view_keeps_the_origin_centered_inside_the_depth_range() {
        let camera = ViewerCamera::default();
        let clip = camera.view_proj(16.0 / 9.0) * glam::Vec4::new(0.0, 0.0, 0.0, 1.0);
        let ndc = clip.truncate() / clip.w;
        assert!(ndc.x.abs() < 1e-5 && ndc.y.abs() < 1e-5);
        assert!(ndc.z > 0.0 && ndc.z < 1.0);
    }

    #[test]
    fn a_piece_that_fits_in_landscape_fits_in_portrait() {
        let camera = ViewerCamera::default();
        let half_short = (camera.fov_deg.to_radians() * 0.5).tan() * 60.0;
        let edge = glam::Vec4::new(half_short * 0.99, 0.0, 0.0, 1.0);
        for aspect in [2.0, 1.0, 0.5] {
            let clip = camera.view_proj(aspect) * edge;
            assert!((clip.x / clip.w).abs() <= 1.0, "aspect {aspect}");
            let clip = camera.view_proj(aspect) * glam::Vec4::new(0.0, half_short * 0.99, 0.0, 1.0);
            assert!((clip.y / clip.w).abs() <= 1.0, "aspect {aspect}");
        }
    }

    #[test]
    fn bad_values_never_produce_a_broken_matrix() {
        let camera = ViewerCamera {
            fov_deg: f32::NAN,
            near_mm: -1.0,
            far_mm: -2.0,
            ..ViewerCamera::default()
        };
        assert!(camera.view_proj(f32::NAN).is_finite());
    }
}
