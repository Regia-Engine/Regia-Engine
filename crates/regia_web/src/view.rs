// SPDX-License-Identifier: MPL-2.0
//! The drag view of a product: `regia_viewer_view` behind the player's trait.

use regia_viewer_view::range;
use regia_viewer_view::{DragView, PointerKind, ViewState};

use crate::WebError;
use crate::product::PackageProduct;
use crate::seams::{CameraPose, PointerClass, ViewModel, ViewValues};

/// A product's drag view.
pub struct ProductView {
    view: DragView,
}

impl ProductView {
    /// The drag view of an opened product: limits and lens from the recipe's `view_range` and
    /// `camera_default`, turning about the middle of the piece, with the default view framing the whole
    /// piece on any canvas. The product learns the distance, so its builds report the real camera.
    pub fn for_product(product: &mut PackageProduct) -> Result<Self, WebError> {
        let (range, mut camera) = range::from_recipe(product.recipe_doc())
            .map_err(|e| WebError::new("package_invalid", format!("recipe view: {e}")))?;
        let lens = 2.0 * (camera.sensor_mm * 0.5 / camera.focal_length_mm.max(0.01)).atan();
        if let Some(distance) = product.fit_distance(lens.to_degrees(), range.tilt_max_deg)? {
            camera.distance_mm = distance;
        }
        product.set_camera_distance(camera.distance_mm);
        let pivot = product.pivot()?;
        let mut view = DragView::new(range, camera);
        view.controller.set_pivot(glam::Vec3::from(pivot));
        Ok(ProductView { view })
    }
}

impl ProductView {
    /// `view` inside the limits at the current viewport.
    pub fn clamped(&self, view: ViewValues) -> ViewValues {
        let mut state = ViewState {
            tilt_deg: view.tilt,
            spin_deg: view.spin,
            pan_mm: view.pan,
            zoom: view.zoom,
        };
        self.view.controller.clamp_state(&mut state);
        values(state)
    }

    /// Why a pose from a camera path breaks the limits at the current viewport, or `None`.
    pub fn pose_outside(&self, pose: &CameraPose) -> Option<String> {
        self.view.controller.pose_outside(&regia_viewer_view::ViewPose {
            eye_mm: glam::Vec3::from(pose.eye),
            look_at_mm: glam::Vec3::from(pose.look_at),
            up: glam::Vec3::from(pose.up),
            fov_y_rad: pose.fov_short_deg.to_radians(),
            fov_short_deg: pose.fov_short_deg,
            near_mm: pose.near,
            far_mm: pose.far,
        })
    }
}

fn values(s: ViewState) -> ViewValues {
    ViewValues {
        tilt: s.tilt_deg,
        spin: s.spin_deg,
        pan: s.pan_mm,
        zoom: s.zoom,
    }
}

impl ViewModel for ProductView {
    fn pointer_down(&mut self, id: u64, class: PointerClass, x: f32, y: f32, t_ms: f64) {
        let kind = match class {
            PointerClass::Touch => PointerKind::Touch,
            PointerClass::Mouse => PointerKind::MousePrimary,
            PointerClass::MouseSecondary => PointerKind::MouseSecondary,
        };
        self.view.pointer_down(id, kind, x, y, t_ms);
    }

    fn pointer_move(&mut self, id: u64, x: f32, y: f32, t_ms: f64) {
        self.view.pointer_move(id, x, y, t_ms);
    }

    fn pointer_up(&mut self, id: u64, t_ms: f64) {
        self.view.pointer_up(id, t_ms);
    }

    fn pointer_cancel(&mut self, id: u64) {
        self.view.pointer_cancel(id);
    }

    fn wheel(&mut self, lines: f32) {
        self.view.wheel(lines);
    }

    fn set_viewport(&mut self, width: f32, height: f32) {
        self.view.controller.set_viewport(width, height);
    }

    fn set_pixel_ratio(&mut self, ratio: f32) {
        self.view.gestures.set_pixel_ratio(ratio);
    }

    fn update(&mut self, dt_s: f32) -> bool {
        self.view.update(dt_s)
    }

    fn is_animating(&self) -> bool {
        self.view.is_animating()
    }

    fn set_view(&mut self, view: ViewValues, immediate: bool) -> ViewValues {
        values(self.view.controller.set_view(
            ViewState {
                tilt_deg: view.tilt,
                spin_deg: view.spin,
                pan_mm: view.pan,
                zoom: view.zoom,
            },
            immediate,
        ))
    }

    fn reset(&mut self) {
        self.view.controller.reset();
    }

    fn shown(&self) -> ViewValues {
        values(self.view.controller.shown())
    }

    fn pose(&self) -> CameraPose {
        let p = self.view.pose();
        CameraPose {
            eye: p.eye_mm.to_array(),
            look_at: p.look_at_mm.to_array(),
            up: p.up.to_array(),
            fov_short_deg: p.fov_short_deg,
            near: p.near_mm,
            far: p.far_mm,
        }
    }
}
