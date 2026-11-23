// SPDX-License-Identifier: MIT-0
//! The drag view with the piece moved into the part of the window the panel leaves free. The camera moves
//! sideways with its target, so nothing turns: the piece just sits off the window's center.

use std::cell::Cell;
use std::rc::Rc;

use regia_web::seams::{CameraPose, PointerClass, ViewModel, ViewValues};

/// A [`ViewModel`] whose camera is offset by a shift the host keeps up to date.
pub struct ShiftedView<V: ViewModel> {
    inner: V,
    /// Fractions of the window's width and height, +x right, +y up.
    shift: Rc<Cell<[f32; 2]>>,
    aspect: f32,
}

impl<V: ViewModel> ShiftedView<V> {
    /// Wraps a view; `shift` is shared with the host.
    pub fn new(inner: V, shift: Rc<Cell<[f32; 2]>>) -> Self {
        Self {
            inner,
            shift,
            aspect: 1.0,
        }
    }
}

/// `pose` with its eye and target moved so the piece lands `shift` away on a window of `aspect`.
#[must_use]
pub fn shifted_pose(pose: CameraPose, aspect: f32, shift: [f32; 2]) -> CameraPose {
    if shift == [0.0, 0.0] {
        return pose;
    }
    let aspect = if aspect.is_finite() && aspect > 0.0 {
        aspect
    } else {
        1.0
    };
    let half_short = (pose.fov_short_deg.to_radians() * 0.5).tan();
    // The field of view runs across the shorter side.
    let (half_x, half_y) = if aspect >= 1.0 {
        (half_short * aspect, half_short)
    } else {
        (half_short, half_short / aspect)
    };
    let eye = glam::Vec3::from(pose.eye);
    let target = glam::Vec3::from(pose.look_at);
    let up = glam::Vec3::from(pose.up);
    let distance = (eye - target).length();
    let right = (target - eye).cross(up).normalize_or_zero();
    // Moving the camera one way moves the piece the other way on screen.
    let offset = right * (-shift[0] * 2.0 * half_x * distance) + up * (-shift[1] * 2.0 * half_y * distance);
    CameraPose {
        eye: (eye + offset).to_array(),
        look_at: (target + offset).to_array(),
        ..pose
    }
}

impl<V: ViewModel> ViewModel for ShiftedView<V> {
    fn pointer_down(&mut self, id: u64, class: PointerClass, x: f32, y: f32, t_ms: f64) {
        self.inner.pointer_down(id, class, x, y, t_ms);
    }

    fn pointer_move(&mut self, id: u64, x: f32, y: f32, t_ms: f64) {
        self.inner.pointer_move(id, x, y, t_ms);
    }

    fn pointer_up(&mut self, id: u64, t_ms: f64) {
        self.inner.pointer_up(id, t_ms);
    }

    fn pointer_cancel(&mut self, id: u64) {
        self.inner.pointer_cancel(id);
    }

    fn wheel(&mut self, lines: f32) {
        self.inner.wheel(lines);
    }

    fn set_viewport(&mut self, width: f32, height: f32) {
        self.aspect = width / height.max(1.0);
        self.inner.set_viewport(width, height);
    }

    fn set_pixel_ratio(&mut self, ratio: f32) {
        self.inner.set_pixel_ratio(ratio);
    }

    fn update(&mut self, dt_s: f32) -> bool {
        self.inner.update(dt_s)
    }

    fn is_animating(&self) -> bool {
        self.inner.is_animating()
    }

    fn set_view(&mut self, view: ViewValues, immediate: bool) -> ViewValues {
        self.inner.set_view(view, immediate)
    }

    fn reset(&mut self) {
        self.inner.reset();
    }

    fn shown(&self) -> ViewValues {
        self.inner.shown()
    }

    fn pose(&self) -> CameraPose {
        shifted_pose(self.inner.pose(), self.aspect, self.shift.get())
    }
}

#[cfg(test)]
mod tests {
    use glam::Vec3;
    use regia_web::scene::viewer_camera;

    use super::*;

    fn pose() -> CameraPose {
        CameraPose {
            eye: [0.0, 0.0, 60.0],
            look_at: [0.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0],
            fov_short_deg: 20.0,
            near: 3.0,
            far: 1200.0,
        }
    }

    #[test]
    fn a_shift_moves_the_piece_on_screen_by_that_fraction() {
        let camera = viewer_camera(&shifted_pose(pose(), 0.5, [0.0, 0.25]));
        let projected = camera.view_proj(0.5).project_point3(Vec3::ZERO);
        // NDC spans 2 per window height: a quarter of the height is 0.5.
        assert!((projected.y - 0.5).abs() < 1e-3, "{projected:?}");
        assert!(projected.x.abs() < 1e-3, "{projected:?}");
        let camera = viewer_camera(&shifted_pose(pose(), 2.0, [-0.2, 0.0]));
        let projected = camera.view_proj(2.0).project_point3(Vec3::ZERO);
        assert!((projected.x + 0.4).abs() < 1e-3, "{projected:?}");
        assert!(projected.y.abs() < 1e-3, "{projected:?}");
    }

    #[test]
    fn no_shift_is_the_same_pose() {
        assert_eq!(shifted_pose(pose(), 0.5, [0.0, 0.0]), pose());
    }

    #[test]
    fn a_shift_keeps_the_viewing_direction() {
        let shifted = shifted_pose(pose(), 0.5, [0.1, 0.2]);
        let direction = Vec3::from(shifted.look_at) - Vec3::from(shifted.eye);
        assert!(
            (direction - Vec3::new(0.0, 0.0, -60.0)).length() < 1e-3,
            "{direction:?}"
        );
    }
}
