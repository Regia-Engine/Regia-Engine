// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from macroquad (https://github.com/not-fl3/macroquad, commit 8d602d3),
// Copyright (c) 2019-2021 Fedor Logachev, MIT OR Apache-2.0, used under the MIT licence.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! The 2D camera: world ↔ clip ↔ screen conversions and the pixel-perfect letterbox
//! (`docs/specs/22-tier2.md` § T2.10).
//!
//! [`Camera2dView`] is macroquad's `Camera2D` adapted to Regia Engine: the world is **+Y up** (so the
//! y-flip macroquad applies for its y-down screen camera is gone), rotation is in radians, and the
//! viewport is passed in explicitly instead of read from a global window. [`OrthoCamera`] is the
//! game-facing form (a centre, pixels per world unit and a rotation) that produces a view for a
//! given output size. [`PixelPerfect`] renders to a fixed virtual resolution and scales it by the
//! largest **integer** factor that fits the window, centred with letterbox bars — macroquad's
//! `letterbox` example with the scale floored to an integer so every virtual pixel becomes an
//! exact `scale`×`scale` block.
//!
//! `regia_framework::camera::ortho` drives these from an ECS `Camera2d` component.

use regia_core::math::{Mat4, UVec2, Vec2, Vec3};

/// A 2D camera transform: world → clip space (macroquad's `Camera2D` matrix, +Y up).
///
/// The camera is moved to `target`, rotated by `rotation`, scaled by `zoom` and finally offset by
/// `offset` (in clip units). `zoom` is clip units per world unit, so a camera that shows a `w`×`h`
/// world rectangle has `zoom = (2 / w, 2 / h)` ([`Camera2dView::from_display_rect`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera2dView {
    /// Rotation in radians. A positive angle turns the world counter-clockwise on screen (a point to
    /// the right of `target` moves towards the top).
    pub rotation: f32,
    /// Clip units per world unit on each axis.
    pub zoom: Vec2,
    /// Rotation and zoom origin, in world units: the world point at the centre of the view
    /// (when `offset` is zero).
    pub target: Vec2,
    /// Displacement of `target` from the view centre, in clip units.
    pub offset: Vec2,
}

impl Default for Camera2dView {
    fn default() -> Self {
        Self {
            rotation: 0.0,
            zoom: Vec2::ONE,
            target: Vec2::ZERO,
            offset: Vec2::ZERO,
        }
    }
}

/// An axis-aligned rectangle in world or screen units (`min` corner + size).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect2 {
    /// Minimum corner (left; bottom in world space, top in screen space).
    pub min: Vec2,
    /// Width and height.
    pub size: Vec2,
}

impl Rect2 {
    /// A rectangle from its minimum corner and size.
    #[must_use]
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            min: Vec2::new(x, y),
            size: Vec2::new(w, h),
        }
    }

    /// The centre point.
    #[must_use]
    pub fn center(&self) -> Vec2 {
        self.min + self.size * 0.5
    }

    /// True when `p` lies inside (min inclusive, max exclusive).
    #[must_use]
    pub fn contains(&self, p: Vec2) -> bool {
        p.x >= self.min.x
            && p.y >= self.min.y
            && p.x < self.min.x + self.size.x
            && p.y < self.min.y + self.size.y
    }
}

impl Camera2dView {
    /// A camera whose view is exactly `rect` (world units, `min` = bottom-left). A zero width or
    /// height counts as `f32::MIN_POSITIVE` so the zoom stays finite (an extreme zoom, not exact).
    #[must_use]
    pub fn from_display_rect(rect: Rect2) -> Self {
        // A zero-sized rect would make the zoom, and every screen conversion, infinite.
        let extent = |size: f32| if size == 0.0 { f32::MIN_POSITIVE } else { size };
        Self {
            target: rect.center(),
            zoom: Vec2::new(2.0 / extent(rect.size.x), 2.0 / extent(rect.size.y)),
            offset: Vec2::ZERO,
            rotation: 0.0,
        }
    }

    /// World → clip matrix. Composed as macroquad's: translate by `-target`, rotate, scale by
    /// `zoom`, translate by `offset`.
    #[must_use]
    pub fn matrix(&self) -> Mat4 {
        let origin = Mat4::from_translation(Vec3::new(-self.target.x, -self.target.y, 0.0));
        let rotation = Mat4::from_rotation_z(self.rotation);
        let scale = Mat4::from_scale(Vec3::new(self.zoom.x, self.zoom.y, 1.0));
        let translation = Mat4::from_translation(Vec3::new(self.offset.x, self.offset.y, 0.0));
        translation * (scale * rotation) * origin
    }

    /// Screen position (pixels, origin top-left, +Y down) of the world `point`, inside a viewport
    /// rectangle given in the same screen pixels.
    #[must_use]
    pub fn world_to_screen(&self, point: Vec2, viewport: Rect2) -> Vec2 {
        let clip = self.matrix().transform_point3(Vec3::new(point.x, point.y, 0.0));
        Vec2::new(
            (clip.x * 0.5 + 0.5) * viewport.size.x + viewport.min.x,
            (0.5 - clip.y * 0.5) * viewport.size.y + viewport.min.y,
        )
    }

    /// World position under the screen `point` (pixels, origin top-left, +Y down; often the
    /// mouse), inside a viewport rectangle in the same screen pixels.
    #[must_use]
    pub fn screen_to_world(&self, point: Vec2, viewport: Rect2) -> Vec2 {
        let clip = Vec2::new(
            (point.x - viewport.min.x) / viewport.size.x * 2.0 - 1.0,
            1.0 - (point.y - viewport.min.y) / viewport.size.y * 2.0,
        );
        let world = self
            .matrix()
            .inverse()
            .transform_point3(Vec3::new(clip.x, clip.y, 0.0));
        Vec2::new(world.x, world.y)
    }
}

/// The game-facing orthographic camera: what the view is centred on, how many output pixels one
/// world unit covers, and a rotation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OrthoCamera {
    /// World point at the centre of the view.
    pub center: Vec2,
    /// Output pixels per world unit (zoom). `1.0` makes one world unit one pixel.
    pub pixels_per_unit: f32,
    /// Rotation in radians.
    pub rotation: f32,
}

impl Default for OrthoCamera {
    fn default() -> Self {
        Self {
            center: Vec2::ZERO,
            pixels_per_unit: 1.0,
            rotation: 0.0,
        }
    }
}

impl OrthoCamera {
    /// The [`Camera2dView`] for an output of `size` pixels.
    #[must_use]
    pub fn view(&self, size: Vec2) -> Camera2dView {
        let ppu = self.pixels_per_unit.max(f32::MIN_POSITIVE);
        Camera2dView {
            rotation: self.rotation,
            zoom: Vec2::new(2.0 * ppu / size.x.max(1.0), 2.0 * ppu / size.y.max(1.0)),
            target: self.center,
            offset: Vec2::ZERO,
        }
    }

    /// The world rectangle an unrotated view of `size` pixels shows.
    #[must_use]
    pub fn visible_rect(&self, size: Vec2) -> Rect2 {
        let half = size / (2.0 * self.pixels_per_unit.max(f32::MIN_POSITIVE));
        Rect2 {
            min: self.center - half,
            size: half * 2.0,
        }
    }

    /// The same camera with its centre moved so the view's left/bottom edge falls on a whole
    /// output pixel of an output `size` pixels big: sprites at integer world positions then map
    /// to integer pixels and never shimmer while the camera scrolls. A rotated camera has no such
    /// edge and comes back unchanged.
    #[must_use]
    pub fn snapped(&self, size: UVec2) -> Self {
        if self.rotation != 0.0 {
            return *self;
        }
        let ppu = self.pixels_per_unit.max(f32::MIN_POSITIVE);
        let half = Vec2::new(size.x as f32, size.y as f32) * 0.5;
        let edge = (self.center * ppu - half).round();
        Self {
            center: (edge + half) / ppu,
            ..*self
        }
    }
}

/// Where a pixel-perfect virtual screen lands in the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Letterbox {
    /// Integer scale: every virtual pixel is a `scale`×`scale` block of window pixels (≥ 1).
    pub scale: u32,
    /// Left edge of the scaled image in window pixels.
    pub x: u32,
    /// Top edge of the scaled image in window pixels.
    pub y: u32,
    /// Width of the scaled image in window pixels (`virtual width × scale`, clipped to the window).
    pub width: u32,
    /// Height of the scaled image in window pixels.
    pub height: u32,
}

/// The ortho pixel-perfect mode: draw at a fixed virtual resolution, scale up by an integer
/// factor, centre with letterbox bars.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelPerfect {
    /// The virtual resolution the game is drawn at (e.g. 320×180).
    pub virtual_size: UVec2,
    /// Colour of the bars around the scaled image (sRGB-encoded RGBA, like `DrawSettings::clear`; converted for an sRGB swapchain).
    pub bar_color: [f32; 4],
}

impl PixelPerfect {
    /// A pixel-perfect mode at `width`×`height` with black bars.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            virtual_size: UVec2::new(width.max(1), height.max(1)),
            bar_color: [0.0, 0.0, 0.0, 1.0],
        }
    }

    /// The largest integer scale that fits `window`, at least 1 (a window smaller than the virtual
    /// screen shows its centre, cropped).
    #[must_use]
    pub fn scale_for(&self, window: UVec2) -> u32 {
        let sx = window.x / self.virtual_size.x.max(1);
        let sy = window.y / self.virtual_size.y.max(1);
        sx.min(sy).max(1)
    }

    /// Where the scaled virtual screen sits in a window of `window` pixels (macroquad's
    /// `letterbox` example with an integer scale; the offsets are whole pixels).
    #[must_use]
    pub fn letterbox(&self, window: UVec2) -> Letterbox {
        let scale = self.scale_for(window);
        let w = self.virtual_size.x * scale;
        let h = self.virtual_size.y * scale;
        let x = window.x.saturating_sub(w) / 2;
        let y = window.y.saturating_sub(h) / 2;
        Letterbox {
            scale,
            x,
            y,
            width: w.min(window.x),
            height: h.min(window.y),
        }
    }

    /// The virtual-screen pixel (origin top-left, +Y down) under a window pixel, or `None` on the
    /// bars. Mouse input for the virtual screen goes through this.
    #[must_use]
    pub fn window_to_virtual(&self, window: UVec2, point: Vec2) -> Option<Vec2> {
        let lb = self.letterbox(window);
        let scale = lb.scale as f32;
        let full = Vec2::new(
            (self.virtual_size.x * lb.scale) as f32,
            (self.virtual_size.y * lb.scale) as f32,
        );
        // A margin is whole pixels (the letterbox rectangle uses integer division); a crop shows
        // the centre, and the blit centres on the exact half pixel, so it is not floored.
        let axis = |window: u32, full: f32| {
            let half = (window as f32 - full) * 0.5;
            if half >= 0.0 { half.floor() } else { half }
        };
        let origin = Vec2::new(axis(window.x, full.x), axis(window.y, full.y));
        let v = (point - origin) / scale;
        let size = Vec2::new(self.virtual_size.x as f32, self.virtual_size.y as f32);
        (v.x >= 0.0 && v.y >= 0.0 && v.x < size.x && v.y < size.y).then_some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_odd_crop_maps_the_mouse_through_the_exact_half_pixel_the_blit_centres_on() {
        let pp = PixelPerfect::new(320, 180);
        // Window 201x100: the blit shows virtual x 59.5..260.5, so window x 0 is virtual x 59.5.
        let v = pp
            .window_to_virtual(UVec2::new(201, 100), Vec2::new(0.0, 0.0))
            .expect("inside");
        assert!((v.x - 59.5).abs() < 1e-4 && (v.y - 40.0).abs() < 1e-4, "{v:?}");
        // A margin stays whole pixels: window 321 puts the screen at x 0 (integer division).
        let v = pp
            .window_to_virtual(UVec2::new(321, 180), Vec2::new(0.0, 0.0))
            .expect("inside");
        assert!((v.x - 0.0).abs() < 1e-4, "{v:?}");
    }

    fn close(a: Vec2, b: Vec2) -> bool {
        (a - b).abs().max_element() < 1e-3
    }

    #[test]
    fn a_zero_sized_display_rect_keeps_a_finite_zoom() {
        let cam = Camera2dView::from_display_rect(Rect2::new(0.0, 0.0, 0.0, 180.0));
        assert!(cam.zoom.is_finite());
    }

    #[test]
    fn a_rotated_camera_is_not_snapped() {
        let cam = OrthoCamera {
            center: Vec2::new(0.4, 0.0),
            rotation: 0.2,
            pixels_per_unit: 1.0,
        };
        assert_eq!(cam.snapped(UVec2::new(100, 100)).center, cam.center);
        let flat = OrthoCamera { rotation: 0.0, ..cam };
        assert_ne!(flat.snapped(UVec2::new(101, 100)).center, cam.center);
    }

    #[test]
    fn display_rect_maps_corners_to_clip_corners() {
        let cam = Camera2dView::from_display_rect(Rect2::new(0.0, 0.0, 320.0, 180.0));
        let m = cam.matrix();
        let bl = m.transform_point3(Vec3::new(0.0, 0.0, 0.0));
        let tr = m.transform_point3(Vec3::new(320.0, 180.0, 0.0));
        assert!(close(Vec2::new(bl.x, bl.y), Vec2::new(-1.0, -1.0)));
        assert!(close(Vec2::new(tr.x, tr.y), Vec2::new(1.0, 1.0)));
    }

    #[test]
    fn screen_and_world_round_trip_with_rotation_and_offset() {
        let cam = Camera2dView {
            rotation: 0.4,
            zoom: Vec2::new(0.01, 0.02),
            target: Vec2::new(30.0, -12.0),
            offset: Vec2::new(0.1, -0.2),
        };
        let viewport = Rect2::new(10.0, 20.0, 640.0, 360.0);
        for p in [
            Vec2::new(0.0, 0.0),
            Vec2::new(55.5, -3.25),
            Vec2::new(-40.0, 17.0),
        ] {
            let s = cam.world_to_screen(p, viewport);
            assert!(close(cam.screen_to_world(s, viewport), p), "{p} -> {s}");
        }
    }

    #[test]
    fn positive_rotation_turns_the_world_counter_clockwise_on_screen() {
        let cam = Camera2dView {
            rotation: std::f32::consts::FRAC_PI_2,
            zoom: Vec2::new(0.01, 0.01),
            target: Vec2::ZERO,
            offset: Vec2::ZERO,
        };
        let viewport = Rect2::new(0.0, 0.0, 200.0, 200.0);
        // A point right of the target swings to the top of the screen (screen y shrinks).
        assert!(close(
            cam.world_to_screen(Vec2::new(10.0, 0.0), viewport),
            Vec2::new(100.0, 90.0)
        ));
    }

    #[test]
    fn screen_y_points_down_and_world_y_points_up() {
        let cam = OrthoCamera::default().view(Vec2::new(200.0, 100.0));
        let viewport = Rect2::new(0.0, 0.0, 200.0, 100.0);
        // The world origin is the screen centre; +Y world is towards the top of the screen.
        assert!(close(
            cam.world_to_screen(Vec2::ZERO, viewport),
            Vec2::new(100.0, 50.0)
        ));
        assert!(close(
            cam.world_to_screen(Vec2::new(0.0, 10.0), viewport),
            Vec2::new(100.0, 40.0)
        ));
        assert!(close(
            cam.world_to_screen(Vec2::new(10.0, 0.0), viewport),
            Vec2::new(110.0, 50.0)
        ));
    }

    #[test]
    fn ortho_camera_pixels_per_unit_scales_the_view() {
        let cam = OrthoCamera {
            pixels_per_unit: 4.0,
            ..OrthoCamera::default()
        };
        let rect = cam.visible_rect(Vec2::new(320.0, 180.0));
        assert!(close(rect.size, Vec2::new(80.0, 45.0)));
        let view = cam.view(Vec2::new(320.0, 180.0));
        let viewport = Rect2::new(0.0, 0.0, 320.0, 180.0);
        assert!(close(
            view.world_to_screen(Vec2::new(1.0, 0.0), viewport),
            Vec2::new(164.0, 90.0)
        ));
    }

    #[test]
    fn snapping_puts_the_view_edge_on_a_whole_pixel() {
        let cam = OrthoCamera {
            center: Vec2::new(10.37, -4.81),
            pixels_per_unit: 2.0,
            rotation: 0.0,
        };
        for size in [UVec2::new(320, 180), UVec2::new(321, 181)] {
            let s = cam.snapped(size);
            let edge = s.center * 2.0 - Vec2::new(size.x as f32, size.y as f32) * 0.5;
            assert!(close(edge, edge.round()), "{size}: {edge}");
            assert!((s.center - cam.center).abs().max_element() <= 0.25 + 1e-4);
        }
    }

    #[test]
    fn letterbox_uses_the_largest_integer_scale_and_centres() {
        let pp = PixelPerfect::new(320, 180);
        let lb = pp.letterbox(UVec2::new(1280, 720));
        assert_eq!((lb.scale, lb.x, lb.y, lb.width, lb.height), (4, 0, 0, 1280, 720));
        let lb = pp.letterbox(UVec2::new(1366, 768));
        assert_eq!(
            (lb.scale, lb.x, lb.y, lb.width, lb.height),
            (4, 43, 24, 1280, 720)
        );
        let lb = pp.letterbox(UVec2::new(1000, 1000));
        assert_eq!((lb.scale, lb.x, lb.y), (3, 20, 230));
        // Smaller than the virtual screen: scale 1, cropped.
        let lb = pp.letterbox(UVec2::new(200, 100));
        assert_eq!((lb.scale, lb.width, lb.height), (1, 200, 100));
    }

    #[test]
    fn window_to_virtual_inverts_the_letterbox() {
        let pp = PixelPerfect::new(320, 180);
        let window = UVec2::new(1366, 768);
        assert_eq!(pp.window_to_virtual(window, Vec2::new(10.0, 10.0)), None);
        let v = pp.window_to_virtual(window, Vec2::new(43.0 + 8.0, 24.0 + 4.0));
        assert_eq!(v, Some(Vec2::new(2.0, 1.0)));
    }

    #[test]
    fn zero_virtual_size_clamps_and_a_shorter_window_samples_the_centre() {
        let tiny = PixelPerfect::new(0, 0);
        assert_eq!(
            tiny,
            PixelPerfect {
                virtual_size: UVec2::new(1, 1),
                bar_color: [0.0, 0.0, 0.0, 1.0],
            }
        );
        assert_eq!(tiny.scale_for(UVec2::new(0, 0)), 1);
        assert_eq!(tiny.scale_for(UVec2::new(5, 9)), 5);
        let lb = tiny.letterbox(UVec2::new(5, 9));
        assert_eq!((lb.scale, lb.x, lb.y, lb.width, lb.height), (5, 0, 2, 5, 5));
        assert_eq!(
            tiny.window_to_virtual(UVec2::new(5, 9), Vec2::new(0.0, 2.0)),
            Some(Vec2::ZERO)
        );
        assert_eq!(
            tiny.window_to_virtual(UVec2::new(5, 9), Vec2::new(4.0, 2.0)),
            Some(Vec2::new(0.8, 0.0))
        );
        assert_eq!(
            tiny.window_to_virtual(UVec2::new(5, 9), Vec2::new(0.0, 1.0)),
            None
        );
        assert_eq!(
            tiny.window_to_virtual(UVec2::new(5, 9), Vec2::new(5.0, 2.0)),
            None
        );
        let cropped = tiny.letterbox(UVec2::new(0, 0));
        assert_eq!((cropped.scale, cropped.width, cropped.height), (1, 0, 0));

        // 200×100 is shorter than 320×180 by an even number of pixels on both axes, so the
        // centred crop lands on whole virtual pixels. A zero window axis still scales by 1.
        let pp = PixelPerfect::new(320, 180);
        assert_eq!(pp.scale_for(UVec2::new(0, 5_000)), 1);
        let window = UVec2::new(200, 100);
        assert_eq!(pp.scale_for(window), 1);
        let lb = pp.letterbox(window);
        assert_eq!((lb.scale, lb.x, lb.y, lb.width, lb.height), (1, 0, 0, 200, 100));
        assert_eq!(
            pp.window_to_virtual(window, Vec2::new(0.0, 0.0)),
            Some(Vec2::new(60.0, 40.0))
        );
        assert_eq!(
            pp.window_to_virtual(window, Vec2::new(100.0, 50.0)),
            Some(Vec2::new(160.0, 90.0))
        );
        assert_eq!(
            pp.window_to_virtual(window, Vec2::new(140.0, 50.0)),
            Some(Vec2::new(200.0, 90.0))
        );
        assert_eq!(
            pp.window_to_virtual(window, Vec2::new(-60.0, -40.0)),
            Some(Vec2::ZERO)
        );
        assert_eq!(
            pp.window_to_virtual(window, Vec2::new(259.0, 139.0)),
            Some(Vec2::new(319.0, 179.0))
        );
        assert_eq!(pp.window_to_virtual(window, Vec2::new(-60.0, -41.0)), None);
        assert_eq!(pp.window_to_virtual(window, Vec2::new(260.0, -40.0)), None);

        // An odd leftover still floors to the same whole pixel the integer letterbox uses.
        let odd = UVec2::new(1367, 769);
        let lb = pp.letterbox(odd);
        assert_eq!(
            (lb.scale, lb.x, lb.y, lb.width, lb.height),
            (4, 43, 24, 1280, 720)
        );
        assert_eq!(pp.window_to_virtual(odd, Vec2::new(43.0, 24.0)), Some(Vec2::ZERO));
        assert_eq!(pp.window_to_virtual(odd, Vec2::new(42.5, 24.0)), None);
        assert_eq!(
            pp.window_to_virtual(odd, Vec2::new(1319.0, 24.0)),
            Some(Vec2::new(319.0, 0.0))
        );
        assert_eq!(pp.window_to_virtual(odd, Vec2::new(1323.0, 24.0)), None);

        let rect = Rect2::new(1.0, 2.0, 4.0, 6.0);
        assert_eq!(rect.center(), Vec2::new(3.0, 5.0));
        assert!(rect.contains(Vec2::new(1.0, 2.0)));
        assert!(rect.contains(Vec2::new(4.5, 7.5)));
        assert!(!rect.contains(Vec2::new(5.0, 2.0)));
        assert!(!rect.contains(Vec2::new(1.0, 8.0)));
        assert!(!rect.contains(Vec2::new(0.5, 2.0)));
        let empty = Rect2::new(0.0, 0.0, 0.0, 0.0);
        assert_eq!(empty.center(), Vec2::ZERO);
        assert!(!empty.contains(Vec2::ZERO));
    }
}
