// SPDX-License-Identifier: MPL-2.0
//! The damped drag-view controller.
//!
//! Two states are kept: the *target* (where input has put the view, always clamped to the range)
//! and the *shown* state (what is drawn). Each frame the shown state moves toward the target by
//! frame-rate independent exponential smoothing, `s = t + (s - t) * exp(-dt / tau)`. Every limit is
//! a convex set (a disk or ellipse of tilt, a box of pan, an interval of zoom), and a point between
//! two points of a convex set stays in it, so the shown state can never leave the range either.
//! The two wrapping angles (the spin about the front axis and a free orbit's horizontal angle) have
//! no limit and are smoothed the short way round.

use glam::{Mat4, Quat, Vec2, Vec3};

use crate::range::{CameraDefaults, SpinMode, ViewRange};

/// Smoothing time constant of the shown state, in seconds.
pub const SMOOTH_TAU_S: f32 = 0.07;
/// Decay time constant of a fling after release, in seconds.
pub const FLING_TAU_S: f32 = 0.25;
/// Degrees of tilt per pixel of drag, times the smaller viewport side.
pub const DEG_PER_VIEWPORT: f32 = 120.0;
/// Zoom factor per scroll line (one wheel notch).
pub const ZOOM_PER_LINE: f32 = 1.1;
/// The widest zoom (1 = the recipe's default framing).
pub const ZOOM_MIN: f32 = 1.0;
/// The largest step one frame may integrate, in seconds (a stalled tab must not jump).
pub const MAX_DT_S: f32 = 0.1;

/// One input, already turned into a view gesture (see `crate::gesture` for raw pointers).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ViewInput {
    /// One-finger or left-button drag, in pixels (x right, y down): turns the view.
    Orbit {
        /// Horizontal motion in pixels.
        dx_px: f32,
        /// Vertical motion in pixels (down is positive).
        dy_px: f32,
    },
    /// Two-finger or right-button drag, in pixels: moves the view in the front plane.
    Pan {
        /// Horizontal motion in pixels.
        dx_px: f32,
        /// Vertical motion in pixels (down is positive).
        dy_px: f32,
    },
    /// Pinch: multiply the zoom by `scale` (> 1 zooms in).
    Pinch {
        /// Ratio of the finger distances, new over old.
        scale: f32,
    },
    /// Mouse wheel or trackpad scroll, in lines (positive zooms in).
    Scroll {
        /// Lines scrolled.
        lines: f32,
    },
    /// Two-finger twist, in degrees counter-clockwise; turns about the front axis where allowed.
    Twist {
        /// Twist angle in degrees.
        deg: f32,
    },
    /// The last finger lifted after an orbit: keep turning with this velocity, decaying.
    Release {
        /// Horizontal velocity in pixels per second.
        vx_px_s: f32,
        /// Vertical velocity in pixels per second.
        vy_px_s: f32,
    },
    /// A press stops any fling.
    Grab,
    /// Double tap or double click: go back to the default view (animated).
    DoubleTap,
}

/// A view state: what the camera shows, in the piece's front frame.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ViewState {
    /// Tilt in degrees: x turns about the up axis (positive moves the camera to the piece's left,
    /// as when the piece is dragged right), y about the right axis (positive moves it up).
    pub tilt_deg: [f32; 2],
    /// Turn of the piece about its front axis, degrees in (-180, 180]. Positive is
    /// counter-clockwise when looking along the front axis at the piece.
    pub spin_deg: f32,
    /// Offset of the look-at point from the pivot in the front plane, mm (x right, y up).
    pub pan_mm: [f32; 2],
    /// Zoom factor (1 = default framing; larger is closer).
    pub zoom: f32,
}

impl Default for ViewState {
    fn default() -> Self {
        ViewState {
            tilt_deg: [0.0, 0.0],
            spin_deg: 0.0,
            pan_mm: [0.0, 0.0],
            zoom: 1.0,
        }
    }
}

/// A camera pose, ready for a renderer. Lengths are mm in the piece's frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewPose {
    /// Camera position.
    pub eye_mm: Vec3,
    /// The point looked at.
    pub look_at_mm: Vec3,
    /// Camera up vector (unit).
    pub up: Vec3,
    /// Vertical field of view, radians.
    pub fov_y_rad: f32,
    /// Field of view across the shorter side of the viewport, degrees (the lens's field of view at
    /// zoom 1, narrowed by the zoom): a piece that fits stays in view in portrait and landscape.
    pub fov_short_deg: f32,
    /// Near plane distance.
    pub near_mm: f32,
    /// Far plane distance.
    pub far_mm: f32,
}

impl ViewPose {
    /// Right-handed view matrix.
    pub fn view_matrix(&self) -> Mat4 {
        glam::camera::rh::view::look_at_mat4(self.eye_mm, self.look_at_mm, self.up)
    }

    /// Right-handed perspective projection with depth 0..1 (wgpu clip space).
    pub fn projection(&self, aspect: f32) -> Mat4 {
        glam::camera::rh::proj::directx::perspective(
            self.fov_y_rad,
            aspect.max(1e-3),
            self.near_mm,
            self.far_mm,
        )
    }

    /// Unit vector from the look-at point to the eye.
    pub fn view_dir(&self) -> Vec3 {
        (self.eye_mm - self.look_at_mm).normalize_or_zero()
    }
}

fn wrap_deg(a: f32) -> f32 {
    let w = (a + 180.0).rem_euclid(360.0) - 180.0;
    if w <= -180.0 { w + 360.0 } else { w }
}

fn finite_or(x: f32, fallback: f32) -> f32 {
    if x.is_finite() { x } else { fallback }
}

/// Keep a positive finite camera. Anything else is the default for that field, so a NaN
/// distance cannot make every later pose non-finite.
fn usable_camera(camera: CameraDefaults) -> CameraDefaults {
    let fallback = CameraDefaults::default();
    let pos = |x: f32, fallback: f32| {
        if x.is_finite() && x > 0.0 && x < 1.0e6 {
            x
        } else {
            fallback
        }
    };
    CameraDefaults {
        distance_mm: pos(camera.distance_mm, fallback.distance_mm),
        focal_length_mm: pos(camera.focal_length_mm, fallback.focal_length_mm),
        sensor_mm: pos(camera.sensor_mm, fallback.sensor_mm),
        f_stop: pos(camera.f_stop, fallback.f_stop),
    }
}

/// Radial projection of `t` onto the ellipse `(x/a)^2 + (y/b)^2 <= 1`.
///
/// The straight `(t.x/a).powi(2)` overflows for a huge finite `t`, and the reciprocal of
/// that infinity is 0, which would project onto the origin. Scale the normalized coordinates
/// down before squaring so the same formula stays finite.
fn project_ellipse(t: Vec2, a: f32, b: f32) -> Vec2 {
    let nx = t.x / a;
    let ny = t.y / b;
    let e = nx * nx + ny * ny;
    if e <= 1.0 {
        return t;
    }
    let shrink = 1.0 - 1.0e-6;
    if e.is_finite() {
        return t * (e.sqrt().recip() * shrink);
    }
    // Squares overflowed. When the ratios themselves are still finite, scale `t` by
    // 1/hypot(nx, ny). When a ratio overflowed, rebuild the boundary point from a
    // finite stand-in of that direction (scaling `t` would no longer match the ratio).
    let ratios_finite = nx.is_finite() && ny.is_finite();
    let nx = if nx.is_finite() { nx } else { t.x.signum() * 1.0e8 };
    let ny = if ny.is_finite() { ny } else { t.y.signum() * 1.0e8 };
    let m = nx.abs().max(ny.abs()).max(1.0);
    let sx = nx / m;
    let sy = ny / m;
    let inv = (sx * sx + sy * sy).sqrt().recip() * shrink;
    if ratios_finite {
        t * (inv / m)
    } else {
        Vec2::new(sx * inv * a, sy * inv * b)
    }
}

/// The drag-view controller shared by the web player and the Android viewer.
#[derive(Clone, Debug)]
pub struct ViewController {
    range: ViewRange,
    camera: CameraDefaults,
    viewport: Vec2,
    pivot_mm: Vec3,
    target: ViewState,
    shown: ViewState,
    fling: Vec2,
    dirty: bool,
}

impl ViewController {
    /// A controller at the default view. `range` must be validated (`ViewRange::validate`); an
    /// invalid one is replaced by the default range so the view is never unbounded.
    pub fn new(range: ViewRange, camera: CameraDefaults) -> Self {
        let range = if range.validate().is_ok() {
            range
        } else {
            ViewRange::default()
        };
        // A hand-built camera can carry a NaN or a negative length. The recipe parser
        // already refuses those; drop them here too so one bad field cannot poison the pose.
        let camera = usable_camera(camera);
        let mut c = ViewController {
            range,
            camera,
            viewport: Vec2::new(1024.0, 768.0),
            pivot_mm: Vec3::ZERO,
            target: ViewState::default(),
            shown: ViewState::default(),
            fling: Vec2::ZERO,
            dirty: true,
        };
        c.reclamp();
        c
    }

    /// The active range.
    pub fn range(&self) -> &ViewRange {
        &self.range
    }

    /// The camera defaults.
    pub fn camera(&self) -> &CameraDefaults {
        &self.camera
    }

    /// Replace the range (a new product or recipe); the views are clamped into it at once.
    /// An invalid range is refused and the old one kept.
    pub fn set_range(&mut self, range: ViewRange) -> Result<(), crate::range::ViewRangeError> {
        range.validate()?;
        self.range = range;
        self.reclamp();
        self.dirty = true;
        Ok(())
    }

    /// The point the camera turns about (usually the piece's bounds center), in mm.
    pub fn set_pivot(&mut self, pivot_mm: Vec3) {
        if pivot_mm.is_finite() {
            self.pivot_mm = pivot_mm;
            self.dirty = true;
        }
    }

    /// The drawable size in pixels; the zoom limit depends on it (`max_px_per_mm`).
    pub fn set_viewport(&mut self, width_px: f32, height_px: f32) {
        let w = finite_or(width_px, 1.0).clamp(1.0, 65536.0);
        let h = finite_or(height_px, 1.0).clamp(1.0, 65536.0);
        self.viewport = Vec2::new(w, h);
        self.reclamp();
        self.dirty = true;
    }

    /// The viewport in pixels.
    pub fn viewport(&self) -> Vec2 {
        self.viewport
    }

    /// Visible extent of the front plane across the viewport's shorter side at zoom 1, in mm.
    fn visible_short_mm(&self) -> f32 {
        2.0 * self.camera.distance_mm * (self.camera.lens_fov_rad() * 0.5).tan()
    }

    /// Screen pixels per mm of the piece at `zoom`.
    pub fn px_per_mm(&self, zoom: f32) -> f32 {
        self.viewport.min_element() * zoom / self.visible_short_mm().max(1e-6)
    }

    /// The closest zoom the range allows in this viewport (never below `ZOOM_MIN`).
    pub fn zoom_max(&self) -> f32 {
        (self.range.max_px_per_mm / self.px_per_mm(1.0)).max(ZOOM_MIN)
    }

    /// Where input has put the view (always inside the range).
    pub fn target(&self) -> ViewState {
        self.target
    }

    /// What is drawn this frame (always inside the range).
    pub fn shown(&self) -> ViewState {
        self.shown
    }

    /// Whether the shown view still moves toward the target, or a fling is running. A lazy render
    /// loop draws while this is true and sleeps otherwise.
    pub fn is_animating(&self) -> bool {
        self.fling != Vec2::ZERO || !self.close(&self.shown, &self.target)
    }

    /// Whether anything changed since the last call (then clears the flag). Draw when true.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    fn close(&self, a: &ViewState, b: &ViewState) -> bool {
        let d = |x: f32, y: f32| (x - y).abs();
        // A free orbit's yaw wraps. 180 and -179.999 are the same heading; a linear
        // compare never calls them close, and the short-way lerp then sits on the
        // branch cut forever (`is_animating` stays true).
        let yaw = if self.range.spin == SpinMode::Free {
            wrap_deg(a.tilt_deg[0] - b.tilt_deg[0]).abs()
        } else {
            d(a.tilt_deg[0], b.tilt_deg[0])
        };
        yaw < 1e-3
            && d(a.tilt_deg[1], b.tilt_deg[1]) < 1e-3
            && wrap_deg(a.spin_deg - b.spin_deg).abs() < 1e-3
            && d(a.pan_mm[0], b.pan_mm[0]) < 1e-4
            && d(a.pan_mm[1], b.pan_mm[1]) < 1e-4
            && d(a.zoom.ln(), b.zoom.ln()) < 1e-4
    }

    fn deg_per_px(&self) -> f32 {
        DEG_PER_VIEWPORT / self.viewport.min_element().max(1.0)
    }

    fn mm_per_px(&self, zoom: f32) -> f32 {
        1.0 / self.px_per_mm(zoom).max(1e-6)
    }

    /// Apply one input to the target. Non-finite values are ignored.
    pub fn input(&mut self, input: ViewInput) {
        match input {
            ViewInput::Orbit { dx_px, dy_px } => {
                if !(dx_px.is_finite() && dy_px.is_finite()) {
                    return;
                }
                self.fling = Vec2::ZERO;
                let k = self.deg_per_px();
                self.target.tilt_deg[0] += (dx_px * k).clamp(-360.0, 360.0);
                self.target.tilt_deg[1] += (dy_px * k).clamp(-360.0, 360.0);
            }
            ViewInput::Pan { dx_px, dy_px } => {
                if !(dx_px.is_finite() && dy_px.is_finite()) {
                    return;
                }
                let k = self.mm_per_px(self.target.zoom);
                self.target.pan_mm[0] -= (dx_px * k).clamp(-1.0e4, 1.0e4);
                self.target.pan_mm[1] += (dy_px * k).clamp(-1.0e4, 1.0e4);
            }
            ViewInput::Pinch { scale } => {
                if scale.is_finite() && scale > 0.0 {
                    self.target.zoom *= scale.clamp(0.01, 100.0);
                }
            }
            ViewInput::Scroll { lines } => {
                if lines.is_finite() {
                    self.target.zoom *= ZOOM_PER_LINE.powf(lines.clamp(-50.0, 50.0));
                }
            }
            ViewInput::Twist { deg } => {
                if deg.is_finite() && matches!(self.range.spin, SpinMode::AboutFrontAxis | SpinMode::Free) {
                    self.target.spin_deg = wrap_deg(self.target.spin_deg + deg.clamp(-360.0, 360.0));
                }
            }
            ViewInput::Release { vx_px_s, vy_px_s } => {
                if vx_px_s.is_finite() && vy_px_s.is_finite() {
                    let k = self.deg_per_px();
                    let v = Vec2::new(vx_px_s, vy_px_s) * k;
                    // Slow releases do not fling; very fast ones are capped.
                    self.fling = if v.length() < 20.0 {
                        Vec2::ZERO
                    } else {
                        v.clamp_length_max(720.0)
                    };
                }
            }
            ViewInput::Grab => self.fling = Vec2::ZERO,
            ViewInput::DoubleTap => self.reset(),
        }
        self.reclamp();
        self.dirty = true;
    }

    /// Animate back to the default view.
    pub fn reset(&mut self) {
        self.fling = Vec2::ZERO;
        self.target = ViewState::default();
        self.reclamp();
        self.dirty = true;
    }

    /// Set the view directly (the web player's `setView`). The state is clamped into the range;
    /// with `immediate` the shown view jumps there without animating. Returns the clamped state.
    pub fn set_view(&mut self, state: ViewState, immediate: bool) -> ViewState {
        self.fling = Vec2::ZERO;
        let mut s = state;
        let sanitize = |x: f32, d: f32| finite_or(x, d);
        s.tilt_deg = [sanitize(s.tilt_deg[0], 0.0), sanitize(s.tilt_deg[1], 0.0)];
        s.spin_deg = sanitize(s.spin_deg, 0.0);
        s.pan_mm = [sanitize(s.pan_mm[0], 0.0), sanitize(s.pan_mm[1], 0.0)];
        s.zoom = sanitize(s.zoom, 1.0);
        self.target = s;
        self.reclamp();
        if immediate {
            self.shown = self.target;
        }
        self.dirty = true;
        self.target
    }

    /// Clamp `s` into the range. Every limit is convex, so this is a projection.
    pub fn clamp_state(&self, s: &mut ViewState) {
        let r = &self.range;
        match r.spin {
            SpinMode::Free => {
                s.tilt_deg[0] = wrap_deg(finite_or(s.tilt_deg[0], 0.0));
                s.tilt_deg[1] =
                    finite_or(s.tilt_deg[1], 0.0).clamp(-r.vertical_limit_deg(), r.vertical_limit_deg());
            }
            _ => {
                let a = r.horizontal_limit_deg();
                let b = r.vertical_limit_deg();
                let t = Vec2::new(finite_or(s.tilt_deg[0], 0.0), finite_or(s.tilt_deg[1], 0.0));
                let t = if a <= 0.0 || b <= 0.0 {
                    // A degenerate cone: only the axis that still has room may move.
                    Vec2::new(
                        t.x.clamp(-a.max(0.0), a.max(0.0)),
                        t.y.clamp(-b.max(0.0), b.max(0.0)),
                    )
                } else {
                    // Ellipse (a disk when a == b): scale radially onto the boundary. Shrink a hair
                    // so float error in the scaled value can never land outside.
                    // (t/a)^2 overflows to infinity for a huge but finite tilt (around 1e21 degrees
                    // on a 30 degree cone, and the fuzzer's 1e30). `inf.recip()` is 0, which used
                    // to drop the view on the front instead of the rim.
                    project_ellipse(t, a, b)
                };
                s.tilt_deg = [t.x, t.y];
            }
        }
        s.spin_deg = match r.spin {
            SpinMode::AboutFrontAxis | SpinMode::Free => wrap_deg(finite_or(s.spin_deg, 0.0)),
            _ => 0.0,
        };
        for (pan, [lo, hi]) in s.pan_mm.iter_mut().zip(r.pan_bounds_mm) {
            *pan = finite_or(*pan, 0.0).clamp(lo, hi);
        }
        let zmax = self.zoom_max();
        let z = finite_or(s.zoom, 1.0);
        s.zoom = if z > 0.0 {
            z.clamp(ZOOM_MIN, zmax)
        } else {
            ZOOM_MIN
        };
    }

    fn reclamp(&mut self) {
        let mut t = self.target;
        self.clamp_state(&mut t);
        self.target = t;
        let mut s = self.shown;
        self.clamp_state(&mut s);
        self.shown = s;
    }

    /// Advance by `dt_s` seconds: run the fling, then move the shown view toward the target.
    /// Returns whether the shown view changed (draw this frame).
    pub fn update(&mut self, dt_s: f32) -> bool {
        let dt = if dt_s.is_finite() {
            dt_s.clamp(0.0, MAX_DT_S)
        } else {
            0.0
        };
        let before = self.shown;
        if self.fling != Vec2::ZERO {
            let before_t = self.target.tilt_deg;
            self.target.tilt_deg[0] += self.fling.x * dt;
            self.target.tilt_deg[1] += self.fling.y * dt;
            self.reclamp();
            // A fling that hits a limit stops on that axis.
            if self.range.spin != SpinMode::Free {
                let want = Vec2::from(before_t) + self.fling * dt;
                let got = Vec2::from(self.target.tilt_deg);
                if (got.x - want.x).abs() > 1e-4 {
                    self.fling.x = 0.0;
                }
                if (got.y - want.y).abs() > 1e-4 {
                    self.fling.y = 0.0;
                }
            }
            self.fling *= (-dt / FLING_TAU_S).exp();
            if self.fling.length() < 1.0 {
                self.fling = Vec2::ZERO;
            }
        }
        let k = (-dt / SMOOTH_TAU_S).exp();
        let lerp = |s: f32, t: f32| t + (s - t) * k;
        let free = self.range.spin == SpinMode::Free;
        let mut s = self.shown;
        s.tilt_deg[0] = if free {
            wrap_deg(self.target.tilt_deg[0] + wrap_deg(s.tilt_deg[0] - self.target.tilt_deg[0]) * k)
        } else {
            lerp(s.tilt_deg[0], self.target.tilt_deg[0])
        };
        s.tilt_deg[1] = lerp(s.tilt_deg[1], self.target.tilt_deg[1]);
        s.spin_deg = wrap_deg(self.target.spin_deg + wrap_deg(s.spin_deg - self.target.spin_deg) * k);
        s.pan_mm = [
            lerp(s.pan_mm[0], self.target.pan_mm[0]),
            lerp(s.pan_mm[1], self.target.pan_mm[1]),
        ];
        s.zoom = (self.target.zoom.ln() + (s.zoom.ln() - self.target.zoom.ln()) * k).exp();
        // Float error must not leave the range; convexity makes this a no-op in exact arithmetic.
        self.clamp_state(&mut s);
        if self.close(&s, &self.target) && self.fling == Vec2::ZERO {
            s = self.target;
        }
        self.shown = s;
        let changed = before != self.shown;
        self.dirty |= changed;
        changed
    }

    /// The front-frame basis: (front, up, right), right-handed (right = up x front).
    pub fn basis(&self) -> (Vec3, Vec3, Vec3) {
        let front = self.range.front_axis;
        let seed = if front.dot(Vec3::Y).abs() > 0.99 {
            Vec3::NEG_Z
        } else {
            Vec3::Y
        };
        let right = seed.cross(front).normalize();
        let up = front.cross(right).normalize();
        (front, up, right)
    }

    /// The rotation a view state applies to the front frame.
    pub fn orientation(&self, s: &ViewState) -> Quat {
        let (front, up, right) = self.basis();
        let tx = s.tilt_deg[0].to_radians();
        let ty = s.tilt_deg[1].to_radians();
        // Positive spin_deg is counter-clockwise when looking along the front axis at the
        // piece (the same sign as `ViewInput::Twist`). The camera rolls the other way.
        let spin = Quat::from_axis_angle(front, -s.spin_deg.to_radians());
        let tilt = if self.range.spin == SpinMode::Free {
            Quat::from_axis_angle(up, -tx) * Quat::from_axis_angle(right, -ty)
        } else {
            // Exponential map: one rotation about an axis in the front plane, so the angle between
            // the view direction and the front axis is exactly |tilt|.
            let r = up * -tx + right * -ty;
            let angle = r.length();
            if angle > 0.0 {
                Quat::from_axis_angle(r / angle, angle)
            } else {
                Quat::IDENTITY
            }
        };
        tilt * spin
    }

    /// The pose of a view state.
    pub fn pose_of(&self, s: &ViewState) -> ViewPose {
        let (front, up, right) = self.basis();
        let q = self.orientation(s);
        let look_at = self.pivot_mm + right * s.pan_mm[0] + up * s.pan_mm[1];
        let dist = self.camera.distance_mm;
        let aspect = self.viewport.x / self.viewport.y.max(1.0);
        // The lens's field of view spans the shorter side (the renderer's convention).
        let fov_short = 2.0 * ((self.camera.lens_fov_rad() * 0.5).tan() / s.zoom.max(1e-3)).atan();
        let fov_y = if aspect >= 1.0 {
            fov_short
        } else {
            2.0 * ((fov_short * 0.5).tan() / aspect.max(1e-3)).atan()
        };
        ViewPose {
            eye_mm: look_at + q * front * dist,
            look_at_mm: look_at,
            up: (q * up).normalize(),
            fov_y_rad: fov_y,
            fov_short_deg: fov_short.to_degrees(),
            // The old near plane was at least 0.01 mm, so a valid distance under that
            // (the parser allows any positive number) put the pivot behind the near plane,
            // and under about 0.0005 mm the far plane crossed in front of it.
            near_mm: (dist * 0.05).clamp(dist * 1.0e-4, dist * 0.5),
            far_mm: dist * 20.0,
        }
    }

    /// Why a pose a script gives (a camera path, not this controller) breaks the range, or `None` when it stays
    /// inside: the same limits [`Self::clamp_state`] holds a view state to, read back from the pose. A camera
    /// further away than the default framing is allowed, since only `max_px_per_mm` limits how close it comes.
    /// The tilt, spin and pan compare with a small tolerance for float error; `viewport` sets the pixel density.
    pub fn pose_outside(&self, pose: &ViewPose) -> Option<String> {
        const ANGLE_TOLERANCE_DEG: f32 = 0.02;
        const PAN_TOLERANCE_MM: f32 = 1e-3;
        let (front, up, right) = self.basis();
        let offset = pose.look_at_mm - self.pivot_mm;
        let dir = pose.view_dir();
        if dir == Vec3::ZERO || !pose.up.is_finite() || !pose.eye_mm.is_finite() {
            return Some("the camera has no direction".to_owned());
        }
        let (tx, ty) = if self.range.spin == SpinMode::Free {
            let ty = dir.dot(up).clamp(-1.0, 1.0).asin();
            ((-dir.dot(right)).atan2(dir.dot(front)), ty)
        } else {
            // The exponential map of `orientation`: the rotation axis lies in the front plane.
            let axis = front.cross(dir);
            let along = dir.dot(front).clamp(-1.0, 1.0);
            let angle = along.acos();
            // Looking straight back, front × dir is zero and normalize would drop the
            // 180 degree turn, so the pose read as no tilt and a cone let it through.
            let r = if axis.length_squared() > 1.0e-12 {
                axis.normalize() * angle
            } else if along < 0.0 {
                up * std::f32::consts::PI
            } else {
                Vec3::ZERO
            };
            (-r.dot(up), -r.dot(right))
        };
        // A camera that keeps the piece's up as upright as it can has not spun, whichever way it reached its tilt.
        let upright = (up - dir * up.dot(dir)).normalize_or_zero();
        // The angle between the two ups as atan2(|a x b|, a . b): well conditioned near zero and blind to their
        // lengths. `acos` of the dot amplified rounding there (one ulp under 1 read 0.0198 degrees, two ulps 0.028), so
        // a level camera whose dot rounded two ulps low (the M5, ADR-5550) fell through to the spin reading below.
        let leveled = upright != Vec3::ZERO
            && pose
                .up
                .cross(upright)
                .length()
                .atan2(pose.up.dot(upright))
                .to_degrees()
                <= ANGLE_TOLERANCE_DEG;
        let spin_deg = if leveled {
            0.0
        } else {
            // What is left of the camera's up after the tilt is the spin about the front axis.
            let tilt = self.orientation(&ViewState {
                tilt_deg: [tx.to_degrees(), ty.to_degrees()],
                ..ViewState::default()
            });
            let v = tilt.inverse() * pose.up;
            (-v.dot(right)).atan2(v.dot(up)).to_degrees()
        };
        let state = ViewState {
            tilt_deg: [tx.to_degrees(), ty.to_degrees()],
            spin_deg,
            pan_mm: [offset.dot(right), offset.dot(up)],
            zoom: ZOOM_MIN,
        };
        let mut held = state;
        self.clamp_state(&mut held);
        let tilt_off = Vec2::from(state.tilt_deg).distance(Vec2::from(held.tilt_deg));
        if tilt_off > ANGLE_TOLERANCE_DEG {
            return Some(format!(
                "tilt {:?} degrees is outside the limits (held at {:?})",
                state.tilt_deg, held.tilt_deg
            ));
        }
        if wrap_deg(state.spin_deg - held.spin_deg).abs() > ANGLE_TOLERANCE_DEG {
            return Some(format!(
                "spin {} degrees is outside the limits ({} allows {})",
                state.spin_deg,
                self.range.spin.as_str(),
                held.spin_deg
            ));
        }
        if Vec2::from(state.pan_mm).distance(Vec2::from(held.pan_mm)) > PAN_TOLERANCE_MM {
            return Some(format!(
                "pan {:?} mm is outside the limits (held at {:?})",
                state.pan_mm, held.pan_mm
            ));
        }
        let distance = (pose.eye_mm - pose.look_at_mm).length();
        let visible_short = 2.0 * distance * (pose.fov_short_deg.to_radians() * 0.5).tan();
        let density = self.viewport.min_element() / visible_short.max(1e-6);
        let limit = self.range.max_px_per_mm.max(self.px_per_mm(ZOOM_MIN));
        if density > limit * (1.0 + 1e-4) {
            return Some(format!(
                "{density} pixels per mm is closer than the limit of {limit}"
            ));
        }
        None
    }

    /// The pose to draw this frame.
    pub fn pose(&self) -> ViewPose {
        self.pose_of(&self.shown)
    }

    /// The `camera` block of a `jewelry.build/1` for the target view (interfaces.md § 7):
    /// `rig_preset`, `pan_mm`, `tilt_deg` (the cone angle), `spin_deg`, `zoom`, the lens, plus the
    /// optional `tilt_xy_deg` that keeps the tilt's direction.
    pub fn build_camera(&self) -> serde_json::Value {
        let s = self.target;
        let tilt = Vec2::from(s.tilt_deg);
        let rig = if s.spin_deg != 0.0 {
            "spin"
        } else if tilt != Vec2::ZERO {
            "tilt"
        } else if s.pan_mm != [0.0, 0.0] {
            "pan"
        } else {
            "front"
        };
        let angle = self
            .pose_of(&s)
            .view_dir()
            .dot(self.range.front_axis)
            .clamp(-1.0, 1.0)
            .acos()
            .to_degrees();
        serde_json::json!({
            "rig_preset": rig,
            "pan_mm": s.pan_mm,
            "tilt_deg": angle,
            "tilt_xy_deg": s.tilt_deg,
            "spin_deg": s.spin_deg,
            "zoom": s.zoom,
            "focal_length_mm": self.camera.focal_length_mm * s.zoom,
            "sensor_mm": self.camera.sensor_mm,
            "focus_distance_mm": self.camera.distance_mm,
            "f_stop": self.camera.f_stop,
            "dof": false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctl() -> ViewController {
        ViewController::new(ViewRange::default(), CameraDefaults::default())
    }

    fn settle(c: &mut ViewController) -> usize {
        let mut frames = 0;
        while c.is_animating() && frames < 10_000 {
            c.update(1.0 / 60.0);
            frames += 1;
        }
        frames
    }

    fn free_range() -> ViewRange {
        ViewRange {
            spin: SpinMode::Free,
            ..ViewRange::default()
        }
    }

    #[test]
    fn every_pose_of_an_allowed_view_is_inside_the_range() {
        let spun = ViewRange {
            spin: SpinMode::AboutFrontAxis,
            ..ViewRange::default()
        };
        let cases = [(ViewRange::default(), 0.0), (spun, 40.0), (free_range(), 40.0)];
        for (range, spin) in cases {
            let mut c = ViewController::new(range, CameraDefaults::default());
            c.set_pivot(Vec3::new(1.0, -2.0, 0.5));
            let reach = c.range().tilt_max_deg * 0.7;
            for tilt in [
                [0.0, 0.0],
                [reach, 0.0],
                [-reach * 0.5, reach * 0.5],
                [0.0, -reach],
            ] {
                let state = ViewState {
                    tilt_deg: tilt,
                    spin_deg: spin,
                    pan_mm: [0.5, -0.5],
                    zoom: 1.5,
                };
                let pose = c.pose_of(&state);
                assert_eq!(c.pose_outside(&pose), None, "{tilt:?} spin {spin}");
            }
        }
    }

    #[test]
    fn a_pose_past_a_limit_says_which_limit() {
        let c = ctl();
        let allowed = c.pose_of(&ViewState::default());
        let front = c.basis().0;
        let up = c.basis().1;
        let past_tilt = ViewPose {
            eye_mm: allowed.look_at_mm + (front + up * 2.0).normalize() * 60.0,
            ..allowed
        };
        let reason = c.pose_outside(&past_tilt).expect("past the tilt cone");
        assert!(reason.contains("tilt"), "{reason}");
        let rolled = ViewPose {
            up: c.basis().2,
            ..allowed
        };
        let reason = c.pose_outside(&rolled).expect("this range has no spin");
        assert!(reason.contains("spin"), "{reason}");
        let beside = ViewPose {
            look_at_mm: allowed.look_at_mm + c.basis().2 * 1000.0,
            eye_mm: allowed.eye_mm + c.basis().2 * 1000.0,
            ..allowed
        };
        let reason = c.pose_outside(&beside).expect("past the pan bounds");
        assert!(reason.contains("pan"), "{reason}");
        let close = ViewPose {
            eye_mm: allowed.look_at_mm + front * 0.001,
            ..allowed
        };
        let reason = c.pose_outside(&close).expect("past max_px_per_mm");
        assert!(reason.contains("pixels per mm"), "{reason}");
        let nowhere = ViewPose {
            eye_mm: allowed.look_at_mm,
            ..allowed
        };
        assert!(
            c.pose_outside(&nowhere).is_some(),
            "an eye on its target has no direction"
        );
    }

    #[test]
    fn a_level_look_at_camera_on_a_diagonal_has_not_spun() {
        let c = ctl();
        let (front, up, right) = c.basis();
        let target = Vec3::ZERO;
        let eye = (front * 50.0 + right * 8.0 + up * 4.0) + target;
        let dir = (eye - target).normalize();
        let level = (up - dir * up.dot(dir)).normalize();
        let pose = ViewPose {
            eye_mm: eye,
            look_at_mm: target,
            up: level,
            ..c.pose_of(&ViewState::default())
        };
        assert_eq!(c.pose_outside(&pose), None);
        let rolled = ViewPose {
            up: Quat::from_axis_angle(dir, 5.0_f32.to_radians()) * level,
            ..pose
        };
        let reason = c.pose_outside(&rolled).expect("rolled on a range with no spin");
        assert!(reason.contains("spin"), "{reason}");
    }

    /// ADR-5550: the level test read `acos` of the dot, which turns a dot two ulps under 1 into 0.028 degrees, past the
    /// tolerance; the M5's rounding did that to the published shot list's camera. An up that is level but two ulps short
    /// of unit length gives the same dot on any host.
    #[test]
    fn a_level_up_two_ulps_short_of_unit_length_has_not_spun() {
        let c = ctl();
        let (front, up, right) = c.basis();
        let eye = front * 50.0 + right * 8.0 + up * 4.0;
        let dir = eye.normalize();
        let level = (up - dir * up.dot(dir)).normalize() * (1.0 - 2.0 * f32::EPSILON);
        let pose = ViewPose {
            eye_mm: eye,
            look_at_mm: Vec3::ZERO,
            up: level,
            ..c.pose_of(&ViewState::default())
        };
        assert_eq!(c.pose_outside(&pose), None);
    }

    #[test]
    fn a_camera_further_away_than_the_default_is_allowed() {
        let c = ctl();
        let pose = c.pose_of(&ViewState::default());
        let far = ViewPose {
            eye_mm: pose.look_at_mm + c.basis().0 * 500.0,
            ..pose
        };
        assert_eq!(c.pose_outside(&far), None);
    }

    #[test]
    fn default_pose_looks_down_the_front_axis() {
        let c = ctl();
        let p = c.pose();
        assert!((p.eye_mm - Vec3::new(0.0, 0.0, 60.0)).length() < 1e-4);
        assert!((p.up - Vec3::Y).length() < 1e-5);
        assert!(!c.is_animating());
    }

    #[test]
    fn drag_right_moves_the_camera_left_and_down_drag_moves_it_up() {
        let mut c = ctl();
        c.input(ViewInput::Orbit {
            dx_px: 50.0,
            dy_px: 0.0,
        });
        settle(&mut c);
        assert!(c.pose().eye_mm.x < -1.0);
        c.reset();
        settle(&mut c);
        c.input(ViewInput::Orbit {
            dx_px: 0.0,
            dy_px: 50.0,
        });
        settle(&mut c);
        assert!(c.pose().eye_mm.y > 1.0);
    }

    #[test]
    fn a_long_drag_stops_at_the_cone() {
        let mut c = ctl();
        c.input(ViewInput::Orbit {
            dx_px: 1.0e5,
            dy_px: 3.0e4,
        });
        settle(&mut c);
        let angle = c.pose().view_dir().angle_between(Vec3::Z).to_degrees();
        assert!(angle <= 30.0 + 1e-3 && angle > 29.9, "{angle}");
    }

    #[test]
    fn smoothing_settles_and_then_idles() {
        let mut c = ctl();
        c.input(ViewInput::Orbit {
            dx_px: 40.0,
            dy_px: 10.0,
        });
        let frames = settle(&mut c);
        assert!(frames > 3 && frames < 120, "{frames}");
        assert_eq!(c.shown(), c.target());
        assert!(!c.update(1.0 / 60.0), "a settled view draws nothing");
    }

    #[test]
    fn smoothing_is_frame_rate_independent() {
        let mut a = ctl();
        let mut b = ctl();
        a.input(ViewInput::Orbit {
            dx_px: 60.0,
            dy_px: 0.0,
        });
        b.input(ViewInput::Orbit {
            dx_px: 60.0,
            dy_px: 0.0,
        });
        for _ in 0..6 {
            a.update(1.0 / 30.0);
        }
        for _ in 0..12 {
            b.update(1.0 / 60.0);
        }
        assert!((a.shown().tilt_deg[0] - b.shown().tilt_deg[0]).abs() < 1e-3);
    }

    #[test]
    fn spin_none_ignores_twist_about_front_axis_takes_it() {
        let mut c = ctl();
        c.input(ViewInput::Twist { deg: 45.0 });
        assert_eq!(c.target().spin_deg, 0.0);
        let r = ViewRange {
            spin: SpinMode::AboutFrontAxis,
            ..ViewRange::default()
        };
        c.set_range(r).expect("ok");
        c.input(ViewInput::Twist { deg: 200.0 });
        assert!((c.target().spin_deg - -160.0).abs() < 1e-4);
    }

    #[test]
    fn zoom_is_limited_by_max_px_per_mm() {
        let mut c = ctl();
        c.set_viewport(1000.0, 1000.0);
        c.input(ViewInput::Scroll { lines: 1000.0 });
        let z = c.target().zoom;
        assert!((c.px_per_mm(z) - 400.0).abs() < 0.5, "{}", c.px_per_mm(z));
        c.input(ViewInput::Pinch { scale: 0.0001 });
        assert_eq!(c.target().zoom, ZOOM_MIN);
    }

    #[test]
    fn fling_decays_and_stops_at_the_limit() {
        let mut c = ctl();
        c.input(ViewInput::Release {
            vx_px_s: 4000.0,
            vy_px_s: 0.0,
        });
        assert!(c.is_animating());
        let frames = settle(&mut c);
        assert!(frames < 600);
        let t = c.target().tilt_deg[0];
        assert!(t > 0.0 && t <= 30.0 + 1e-4, "{t}");
    }

    #[test]
    fn double_tap_returns_to_front() {
        let mut c = ctl();
        c.input(ViewInput::Orbit {
            dx_px: 80.0,
            dy_px: 40.0,
        });
        c.input(ViewInput::Pan {
            dx_px: 80.0,
            dy_px: 40.0,
        });
        c.input(ViewInput::Scroll { lines: 3.0 });
        settle(&mut c);
        c.input(ViewInput::DoubleTap);
        settle(&mut c);
        assert_eq!(c.shown(), ViewState::default());
    }

    #[test]
    fn free_orbit_wraps() {
        let r = ViewRange {
            spin: SpinMode::Free,
            ..ViewRange::default()
        };
        let mut c = ViewController::new(r, CameraDefaults::default());
        c.set_view(
            ViewState {
                tilt_deg: [179.0, 0.0],
                ..ViewState::default()
            },
            true,
        );
        c.input(ViewInput::Orbit {
            dx_px: 10.0,
            dy_px: 0.0,
        });
        assert!(c.target().tilt_deg[0] < -170.0);
        // The shown state goes the short way round (never through 0).
        for _ in 0..30 {
            c.update(1.0 / 60.0);
            assert!(c.shown().tilt_deg[0].abs() > 170.0);
        }
    }

    #[test]
    fn build_camera_reports_the_cone_angle() {
        let mut c = ctl();
        c.set_view(
            ViewState {
                tilt_deg: [3.0, 4.0],
                ..ViewState::default()
            },
            true,
        );
        let cam = c.build_camera();
        assert_eq!(cam["rig_preset"], "tilt");
        assert!((cam["tilt_deg"].as_f64().expect("ok") - 5.0).abs() < 1e-3);
    }

    #[test]
    fn non_front_axis_frames_work() {
        let r = ViewRange {
            front_axis: Vec3::Y,
            ..ViewRange::default()
        };
        let c = ViewController::new(r, CameraDefaults::default());
        let p = c.pose();
        assert!((p.view_dir() - Vec3::Y).length() < 1e-5);
        assert!(p.up.dot(Vec3::Y).abs() < 1e-5);
    }

    #[test]
    fn a_camera_looking_straight_back_is_outside_the_cone() {
        let c = ctl();
        let allowed = c.pose_of(&ViewState::default());
        let front = c.basis().0;
        let back = ViewPose {
            eye_mm: allowed.look_at_mm - front * c.camera().distance_mm,
            ..allowed
        };
        let reason = c
            .pose_outside(&back)
            .expect("the back is outside a 30 degree cone");
        assert!(reason.contains("tilt"), "{reason}");
    }

    #[test]
    fn a_rock_that_allows_the_back_accepts_a_straight_back_camera() {
        let range = ViewRange {
            spin: SpinMode::Rock,
            rock_max_deg: Some(180.0),
            ..ViewRange::default()
        };
        let c = ViewController::new(range, CameraDefaults::default());
        let pose = c.pose_of(&ViewState {
            tilt_deg: [180.0, 0.0],
            ..ViewState::default()
        });
        assert_eq!(c.pose_outside(&pose), None, "rock 180 includes the back");
    }

    #[test]
    fn a_huge_finite_tilt_lands_on_the_cone_not_the_front() {
        let mut c = ctl();
        c.set_view(
            ViewState {
                tilt_deg: [1.0e30, 0.0],
                ..ViewState::default()
            },
            true,
        );
        let angle = c.pose().view_dir().angle_between(Vec3::Z).to_degrees();
        assert!(
            (angle - 30.0).abs() < 0.05,
            "radial projection of 1e30 degrees, got {angle}"
        );
        c.set_view(
            ViewState {
                tilt_deg: [1.0e30, 1.0e30],
                ..ViewState::default()
            },
            true,
        );
        let angle = c.pose().view_dir().angle_between(Vec3::Z).to_degrees();
        assert!(
            (angle - 30.0).abs() < 0.05,
            "diagonal overflow still sits on the disk, got {angle}"
        );
        let tilt = Vec2::from(c.target().tilt_deg);
        assert!(
            (tilt.x - tilt.y).abs() < 1e-3,
            "equal components stay on the diagonal, got {tilt}"
        );
    }

    #[test]
    fn a_non_finite_camera_does_not_poison_the_pose() {
        let cam = CameraDefaults {
            distance_mm: f32::NAN,
            focal_length_mm: f32::INFINITY,
            sensor_mm: -1.0,
            f_stop: f32::NAN,
        };
        let c = ViewController::new(ViewRange::default(), cam);
        let p = c.pose();
        assert!(p.eye_mm.is_finite() && p.view_matrix().is_finite(), "{p:?}");
        assert!(p.projection(1.5).is_finite(), "{p:?}");
        assert!((p.eye_mm - Vec3::new(0.0, 0.0, 60.0)).length() < 1e-3);
        assert!(p.near_mm > 0.0 && p.near_mm < p.far_mm, "{p:?}");
    }

    #[test]
    fn a_very_close_camera_keeps_the_pivot_between_the_planes() {
        let cam = CameraDefaults {
            distance_mm: 0.005,
            ..CameraDefaults::default()
        };
        let c = ViewController::new(ViewRange::default(), cam);
        let p = c.pose();
        let dist = (p.eye_mm - p.look_at_mm).length();
        assert!(
            p.near_mm > 0.0 && p.near_mm < dist && dist < p.far_mm,
            "near {} dist {dist} far {}",
            p.near_mm,
            p.far_mm
        );
        assert!(p.projection(1.0).is_finite(), "{p:?}");
        assert!((dist - 0.005).abs() < 1e-6, "{dist}");
    }

    #[test]
    fn a_positive_spin_turns_the_piece_counter_clockwise() {
        // ViewInput::Twist documents positive degrees as counter-clockwise, and that
        // value is stored in spin_deg. Looking down the front axis, a point on the
        // piece's right moves up the screen.
        let range = ViewRange {
            spin: SpinMode::AboutFrontAxis,
            ..ViewRange::default()
        };
        let mut c = ViewController::new(range, CameraDefaults::default());
        c.set_view(
            ViewState {
                spin_deg: 90.0,
                ..ViewState::default()
            },
            true,
        );
        let view = c.pose().view_matrix();
        let on_screen = view.transform_point3(Vec3::X);
        assert!(
            on_screen.y > 0.5 && on_screen.x.abs() < 0.2,
            "right-hand point should rise on screen, view space {on_screen}"
        );
    }

    #[test]
    fn free_orbit_aimed_across_the_back_settles_on_the_target() {
        let mut c = ViewController::new(free_range(), CameraDefaults::default());
        c.set_view(
            ViewState {
                tilt_deg: [-120.0, 0.0],
                ..ViewState::default()
            },
            true,
        );
        c.set_view(
            ViewState {
                tilt_deg: [180.0, 0.0],
                ..ViewState::default()
            },
            false,
        );
        let frames = settle(&mut c);
        assert!(frames < 200, "still animating after {frames}");
        assert_eq!(c.shown().tilt_deg, c.target().tilt_deg);
    }
}
