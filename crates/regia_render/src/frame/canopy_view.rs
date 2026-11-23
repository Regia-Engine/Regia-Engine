// SPDX-License-Identifier: MPL-2.0
//! Which viewer-inside glass draws take the cheap own-canopy shader (ADR-262).
//!
//! [`crate::glass::GlassInstance::inside`] is the host's claim that the camera sits behind the
//! glass. A chase, replay or texture-view camera can leave it set while it looks at the ship from
//! outside, and the cheap shader (`lab_forward.wgsl` `shade_canopy`) has no fog and samples only
//! the first two sun cascades. So the renderer checks the claim per draw against the camera: an
//! eye farther than [`CANOPY_EYE_MARGIN_M`] outside the instance's world bounding sphere draws
//! the full forward shader, whatever the setting. One distance per viewer-inside draw; no state
//! changes for the host.

use super::{FrameMesh, FrameRenderer};

/// Metres past a viewer-inside instance's bounding sphere within which the camera still counts
/// as inside it. The sphere (AABB centre, farthest corner) of a canopy or visor holds the eye
/// already; the margin covers a windscreen-only pane with the pilot's head behind it. A camera
/// past it (a chase camera metres behind the ship) gets the full shader.
pub const CANOPY_EYE_MARGIN_M: f32 = 1.0;

/// Viewer-inside glass draws of the last main-view frame, by the shader their own light took
/// ([`FrameRenderer::canopy_draws`]). Off-screen draws are not counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CanopyDraws {
    /// Drawn with the cheap own-canopy shader.
    pub cheap: u32,
    /// Drawn with the full forward shader because the setting asks for it
    /// ([`crate::glass::CanopyShading::Full`] or `REGIA_CANOPY_FULL`).
    pub full: u32,
    /// Drawn with the full forward shader because the camera was clearly outside the instance's
    /// bounds ([`CANOPY_EYE_MARGIN_M`]) while the setting asked for the cheap one.
    pub eye_outside: u32,
}

impl CanopyDraws {
    pub(super) fn note(&mut self, cheap_setting: bool, eye_outside: bool) {
        let slot = match (cheap_setting, eye_outside) {
            (false, _) => &mut self.full,
            (true, false) => &mut self.cheap,
            (true, true) => &mut self.eye_outside,
        };
        *slot = slot.saturating_add(1);
    }
}

/// Whether `eye` is farther than [`CANOPY_EYE_MARGIN_M`] outside the sphere `(center, radius)`.
/// A non-finite input is not "clearly outside" (the instance keeps the host's claim).
pub(super) fn eye_outside_sphere(eye: [f32; 3], center: [f32; 3], radius: f32) -> bool {
    let d = regia_core::glam::Vec3::from_array(eye).distance(regia_core::glam::Vec3::from_array(center));
    d > radius + CANOPY_EYE_MARGIN_M
}

impl FrameRenderer {
    /// Viewer-inside glass draws of the last main-view frame, by the shader that drew their own
    /// light: the cheap own-canopy shader, the full one by setting, or the full one because the
    /// camera was outside the glass ([`CANOPY_EYE_MARGIN_M`]).
    #[must_use]
    pub fn canopy_draws(&self) -> CanopyDraws {
        self.canopy_draws
    }

    /// Whether the camera at `eye` is clearly outside `mesh`'s world bound: the uploaded bound
    /// under the model matrix for a rigid mesh, the joint-palette bound for a skinned one
    /// (`key` = the submitted mesh key). `false` when the mesh has no bound.
    pub(super) fn canopy_eye_outside(
        &mut self,
        eye: [f32; 3],
        key: u64,
        mesh: &FrameMesh<'_>,
        inflate: f32,
    ) -> bool {
        let sphere = if mesh.joints.is_empty() {
            self.meshes
                .get(&mesh.mesh_key)
                .and_then(|entry| entry.bounds)
                .map(|bounds| {
                    let model = regia_core::glam::Mat4::from_cols_array_2d(&mesh.model);
                    let center = model.transform_point3(regia_core::glam::Vec3::from_array(bounds.center));
                    // Conservative world radius: the largest axis scale (as the frustum test).
                    let scale = (model
                        .x_axis
                        .length_squared()
                        .max(model.y_axis.length_squared())
                        .max(model.z_axis.length_squared()))
                    .sqrt();
                    (center.to_array(), bounds.radius * scale + inflate)
                })
        } else {
            self.skinned_sphere_uncounted(key, mesh, inflate)
        };
        sphere.is_some_and(|(center, radius)| eye_outside_sphere(eye, center, radius))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_margin_separates_a_cockpit_eye_from_a_chase_camera() {
        let (center, radius) = ([0.0, 1.0, 0.0], 1.2);
        // Inside the sphere, on its rim, and just within the margin.
        for eye in [[0.0, 1.0, 0.0], [0.0, 2.2, 0.0], [0.0, 1.0, 2.1]] {
            assert!(!eye_outside_sphere(eye, center, radius), "{eye:?}");
        }
        // Past the margin: a chase camera 6 m behind, a replay camera 2.5 m off the rim.
        for eye in [[0.0, 2.0, 6.0], [3.7, 1.0, 0.0]] {
            assert!(eye_outside_sphere(eye, center, radius), "{eye:?}");
        }
        // Not clearly outside: a NaN eye or bound keeps the host's claim.
        assert!(!eye_outside_sphere([f32::NAN, 0.0, 0.0], center, radius));
        assert!(!eye_outside_sphere([9.0, 0.0, 0.0], center, f32::NAN));
    }

    #[test]
    fn draws_are_counted_by_the_shader_they_took() {
        let mut d = CanopyDraws::default();
        d.note(true, false);
        d.note(true, false);
        d.note(true, true);
        d.note(false, true);
        d.note(false, false);
        assert_eq!(
            d,
            CanopyDraws {
                cheap: 2,
                full: 2,
                eye_outside: 1
            }
        );
    }
}
