// SPDX-License-Identifier: MPL-2.0
//! Camera and draw history across a floating-origin rebase (`docs/specs/78-world-streaming.md`
//! § 12.2). Everything shifted by the same whole number of cells did not move, so motion vectors
//! and temporal history must see no movement on the rebase frame.

use regia_core::math::{Mat4, Vec3};

use super::FrameRenderer;

/// The previous view-projection re-expressed in the shifted space: a point `p` now sits at
/// `p - shift`, so `prev(p) = prev'(p - shift)`.
pub(crate) fn shifted_view_proj(previous: [[f32; 4]; 4], shift: [f32; 3]) -> [[f32; 4]; 4] {
    (Mat4::from_cols_array_2d(&previous) * Mat4::from_translation(Vec3::from_array(shift))).to_cols_array_2d()
}

impl FrameRenderer {
    /// World position of the local origin: the sum of every `shift` given to
    /// [`Self::shift_history`], handed to shared draws (`SharedFrameInfo::origin_offset`).
    #[must_use]
    pub fn origin_offset(&self) -> [f32; 3] {
        self.origin_offset
    }

    /// Sets [`Self::origin_offset`] without shifting anything: for a renderer made after the world's
    /// origin had already moved.
    pub fn set_origin_offset(&mut self, offset: [f32; 3]) {
        self.origin_offset = offset;
    }

    /// Shifts the history of the main view by a rebase; views rendered to textures start their
    /// history over, a one-frame cost that only shows in a render target during a rebase. The
    /// installed irradiance field and reflection probes move with it; the fog and cloud histories
    /// start over.
    pub fn shift_history(&mut self, shift: [f32; 3]) {
        for (axis, amount) in self.origin_offset.iter_mut().zip(shift) {
            *axis += amount;
        }
        if self.have_prev_vp {
            self.prev_view_proj = shifted_view_proj(self.prev_view_proj, shift);
        }
        for model in self.prev_models.values_mut() {
            for (column, amount) in model[3].iter_mut().zip(shift) {
                *column -= amount;
            }
        }
        for view in self.texture_views.values_mut() {
            view.forget_history();
        }
        // The cut detector compares this frame's eye with the last one: a rebase is not a cut.
        self.cut.shift_origin(shift);
        // Installed world-space state the host does not reinstall every frame.
        if let Some(field) = &mut self.gi_field {
            for (axis, amount) in field.origin.iter_mut().zip(shift) {
                *axis -= amount;
            }
        }
        for probe in &mut self.reflection_probes {
            for (axis, amount) in probe.center.iter_mut().zip(shift) {
                *axis -= amount;
            }
        }
        // The fog and cloud histories reproject from the previous eye and drift; they start over,
        // a one-frame cost on the rebase frame like a camera cut's (ADR-2022).
        self.outdoor.reset_history();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec4;

    /// The conversion a world-anchored draw applies: its point projects where the local one does.
    #[test]
    fn world_space_view_proj_projects_a_world_point_where_local_projects_the_local_one() {
        let local = (Mat4::from_rotation_y(0.3) * Mat4::from_translation(Vec3::new(-40.0, -5.0, 30.0)))
            .to_cols_array_2d();
        let offset = [4096.0f32, 0.0, 2048.0];
        let world_vp = shifted_view_proj(local, offset.map(|axis| -axis));
        let point = Vec3::new(20.0, 3.0, 4.0);
        let in_world = point + Vec3::from_array(offset);
        let a = Mat4::from_cols_array_2d(&local) * point.extend(1.0);
        let b = Mat4::from_cols_array_2d(&world_vp) * in_world.extend(1.0);
        assert!((a - b).abs().max_element() < 1.0e-2, "{a} {b}");
    }

    #[test]
    fn a_static_point_projects_to_the_same_clip_position_after_the_shift() {
        let previous = (Mat4::from_rotation_y(0.3)
            * Mat4::from_translation(Vec3::new(-4000.0, -5.0, -3000.0)))
        .to_cols_array_2d();
        let shift = [4096.0, 0.0, 2048.0];
        let shifted = shifted_view_proj(previous, shift);
        let point = Vec3::new(4020.0, 3.0, 3004.0);
        let before = Mat4::from_cols_array_2d(&previous) * point.extend(1.0);
        let moved = point - Vec3::from_array(shift);
        let after = Mat4::from_cols_array_2d(&shifted) * Vec4::new(moved.x, moved.y, moved.z, 1.0);
        assert!((before - after).abs().max_element() < 1.0e-3, "{before} {after}");
    }
}
