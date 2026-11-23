// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/photo_mode.rs, commit c9513623f), ADR-8012.
// Adaptations (ADR-8415): leash radii, speeds, look rate, pitch limit and dolly are
// `PhotoLimits` settings whose defaults equal the source constants; the pictures
// subfolder and key defaults are dropped (named actions, platform pictures directory);
// capture names are `<game>-<map>-t<tick>-<n>.png`; the look direction is written as a
// Regia −Z camera rotation. Presentation only: not in the lockstep closure.

//! Leashed photo mode (ADR-8415).
//!
//! Offline play, training, and a dead or spectating seat roam inside
//! [`FREE_RADIUS_M`]. A live online match that is still in the fight stays inside
//! [`ONLINE_RADIUS_M`] of its anchor. A competitive match cannot open photo mode.
//! The camera never writes sim input, never pauses a match, and reads no wall clock.
//! Capture names carry the sim tick.

#[cfg(feature = "client")]
use std::path::Path;
use std::path::PathBuf;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_input::{Actions, InputInjection};
use regia_scene::Transform;

/// Named action that toggles photo mode. Bind it in the input map; this module stores no key.
pub const TOGGLE_ACTION: &str = "photo_toggle";
/// Named action that writes one still.
pub const CAPTURE_ACTION: &str = "photo_capture";
/// Named action that leaves photo mode. It does not open it.
pub const EXIT_ACTION: &str = "photo_exit";
/// Move stick: x is strafe (right positive), y is forward.
pub const MOVE_ACTION: &str = "photo_move";
/// Vertical axis: positive is up.
pub const UP_ACTION: &str = "photo_up";
/// Look deltas in pixels: x yaw, y pitch (positive looks up).
pub const LOOK_ACTION: &str = "photo_look";
/// Hold-to-hurry.
pub const FAST_ACTION: &str = "photo_fast";
/// Wheel notches. Positive dollies along the look direction.
pub const DOLLY_ACTION: &str = "photo_dolly";

/// Default free-roam leash, metres.
pub const FREE_RADIUS_M: f32 = 30.0;
/// Alias of [`FREE_RADIUS_M`] kept for the source radius tests.
pub const RADIUS_M: f32 = FREE_RADIUS_M;
/// Default live-online leash, metres.
pub const ONLINE_RADIUS_M: f32 = 12.0;
/// Narrowest photo field of view, degrees.
pub const FOV_MIN_DEG: f32 = 20.0;
/// Default photo field of view, degrees.
pub const FOV_DEFAULT_DEG: f32 = 55.0;
/// Widest photo field of view, degrees.
pub const FOV_MAX_DEG: f32 = 120.0;
/// Largest roll either way, degrees.
pub const ROLL_MAX_DEG: f32 = 180.0;
/// Lowest exposure compensation, EV.
pub const EXPOSURE_MIN_EV: f32 = -3.0;
/// Highest exposure compensation, EV.
pub const EXPOSURE_MAX_EV: f32 = 3.0;
/// Nearest focus distance, metres.
pub const FOCUS_MIN_M: f32 = 0.5;
/// Default focus distance, metres.
pub const FOCUS_DEFAULT_M: f32 = 12.0;
/// Farthest focus distance, metres.
pub const FOCUS_MAX_M: f32 = 500.0;
/// Widest aperture, f-stops.
pub const APERTURE_MIN_F: f32 = 1.4;
/// Default aperture, f-stops.
pub const APERTURE_DEFAULT_F: f32 = 5.6;
/// Narrowest aperture, f-stops.
pub const APERTURE_MAX_F: f32 = 16.0;
/// Cruise speed, metres per second.
pub const MOVE_SPEED_M_S: f32 = 6.0;
/// Held-fast speed multiplier.
pub const FAST_MULT: f32 = 4.0;
/// Radians of aim per mouse pixel.
pub const LOOK_RAD_PER_PX: f32 = 0.0022;
/// Pitch clamp, radians.
pub const PITCH_LIMIT_RAD: f32 = 1.553;
/// Metres the wheel dollies per notch.
pub const DOLLY_PER_NOTCH_M: f32 = 1.0;
/// Hi-res capture scale.
pub const CAPTURE_SCALE_HIRES: u32 = 2;
/// Longest capture side, pixels.
pub const CAPTURE_MAX_DIM: u32 = 7680;
/// Frame size used when the window size is zero.
pub const FALLBACK_SIZE: (u32, u32) = (1280, 720);
/// How many numbered names a capture will probe before it stops.
pub const CAPTURE_NAME_PROBE_CAP: u32 = 100_000;

/// What kind of match photo mode is opening into.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MatchAccess {
    /// A match with other seats.
    pub networked: bool,
    /// A competitive match. Photo mode may not open.
    pub competitive: bool,
    /// The sim is already paused. Photo mode itself never sets this.
    pub paused: bool,
    /// A combat exercise. Same roam as offline.
    pub training: bool,
    /// This seat's body is down.
    pub dead: bool,
    /// This seat is spectating.
    pub spectating: bool,
}

/// How far the eye may leave the anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Roam {
    /// Offline, training, dead, spectating, or an already-paused solo sim.
    Free,
    /// A live online seat that is still in the fight.
    Online,
}

/// Whether the toggle may open photo mode. Competitive matches never may.
#[must_use]
pub fn allowed(access: MatchAccess) -> bool {
    !access.competitive
}

/// The leash for this seat. Live online and still fighting is the short one.
#[must_use]
pub fn roam_of(access: MatchAccess) -> Roam {
    let wide = !access.networked || access.training || access.dead || access.spectating || access.paused;
    if wide { Roam::Free } else { Roam::Online }
}

/// Metres the eye may sit from the anchor for `roam`, using the default radii.
#[must_use]
pub fn radius_for(roam: Roam) -> f32 {
    match roam {
        Roam::Free => FREE_RADIUS_M,
        Roam::Online => ONLINE_RADIUS_M,
    }
}

/// Photo mode never pauses the sim.
#[must_use]
pub fn pauses_sim(_access: MatchAccess) -> bool {
    false
}

/// Why an entry attempt was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DenyReason {
    /// A competitive match.
    Competitive,
    /// Kept so a caller can name the old refusal. A live online match no longer
    /// returns this: it opens on [`Roam::Online`].
    NetworkedLive,
}

/// The gameplay view photo mode displaces.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SavedView {
    /// Gameplay field of view, degrees.
    pub fov_deg: f32,
    /// Whether the HUD was showing.
    pub hud_visible: bool,
}

/// One colour grade preset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grade {
    /// Overlay name.
    pub name: &'static str,
    /// Colour strength multiplier.
    pub saturation: f32,
    /// White-point shift, cool (−1) to warm (+1).
    pub warmth: f32,
    /// Extra exposure on top of the photo compensation, EV.
    pub lift_ev: f32,
}

/// Grade list in overlay order.
pub const GRADES: &[Grade] = &[
    Grade {
        name: "NEUTRAL",
        saturation: 1.0,
        warmth: 0.0,
        lift_ev: 0.0,
    },
    Grade {
        name: "FIRST LIGHT",
        saturation: 1.05,
        warmth: 0.3,
        lift_ev: 0.15,
    },
    Grade {
        name: "DUSK",
        saturation: 1.1,
        warmth: 0.55,
        lift_ev: -0.15,
    },
    Grade {
        name: "OVERCAST",
        saturation: 0.9,
        warmth: -0.25,
        lift_ev: 0.1,
    },
    Grade {
        name: "MONO",
        saturation: 0.0,
        warmth: 0.0,
        lift_ev: 0.0,
    },
];

/// Focus distance and aperture for the renderer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DepthOfField {
    /// Focus distance, metres.
    pub focus_m: f32,
    /// Aperture, f-stops.
    pub aperture_f: f32,
}

/// Move rates for one frame. Each axis is −1..=1. `fast` is a hold, not a rate.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PhotoMove {
    /// Forward (+) / back (−) along the look direction.
    pub fwd: f32,
    /// Right (+) / left (−).
    pub strafe: f32,
    /// Up (+) / down (−).
    pub up: f32,
    /// Hold-to-hurry.
    pub fast: bool,
}

fn finite_or(rate: f32, fallback: f32) -> f32 {
    if rate.is_finite() { rate } else { fallback }
}

fn clamp_rate(rate: f32) -> f32 {
    finite_or(rate, 0.0).clamp(-1.0, 1.0)
}

/// Tunables whose defaults equal the source constants.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhotoLimits {
    /// Free-roam leash, metres.
    pub free_radius_m: f32,
    /// Live-online leash, metres.
    pub online_radius_m: f32,
    /// Cruise speed, metres per second.
    pub move_speed_m_s: f32,
    /// Fast multiplier.
    pub fast_mult: f32,
    /// Radians of aim per pixel.
    pub look_rad_per_px: f32,
    /// Pitch clamp, radians.
    pub pitch_limit_rad: f32,
    /// Metres per dolly notch.
    pub dolly_per_notch_m: f32,
    /// Narrowest field of view, degrees.
    pub fov_min_deg: f32,
    /// Widest field of view, degrees.
    pub fov_max_deg: f32,
    /// Largest roll either way, degrees.
    pub roll_max_deg: f32,
    /// Lowest exposure compensation, EV.
    pub exposure_min_ev: f32,
    /// Highest exposure compensation, EV.
    pub exposure_max_ev: f32,
    /// Nearest focus, metres.
    pub focus_min_m: f32,
    /// Farthest focus, metres.
    pub focus_max_m: f32,
    /// Widest aperture, f-stops.
    pub aperture_min_f: f32,
    /// Narrowest aperture, f-stops.
    pub aperture_max_f: f32,
}

impl Default for PhotoLimits {
    fn default() -> Self {
        Self {
            free_radius_m: FREE_RADIUS_M,
            online_radius_m: ONLINE_RADIUS_M,
            move_speed_m_s: MOVE_SPEED_M_S,
            fast_mult: FAST_MULT,
            look_rad_per_px: LOOK_RAD_PER_PX,
            pitch_limit_rad: PITCH_LIMIT_RAD,
            dolly_per_notch_m: DOLLY_PER_NOTCH_M,
            fov_min_deg: FOV_MIN_DEG,
            fov_max_deg: FOV_MAX_DEG,
            roll_max_deg: ROLL_MAX_DEG,
            exposure_min_ev: EXPOSURE_MIN_EV,
            exposure_max_ev: EXPOSURE_MAX_EV,
            focus_min_m: FOCUS_MIN_M,
            focus_max_m: FOCUS_MAX_M,
            aperture_min_f: APERTURE_MIN_F,
            aperture_max_f: APERTURE_MAX_F,
        }
    }
}

impl PhotoLimits {
    /// Replace a non-finite or non-positive radius or speed with the default.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let base = Self::default();
        let positive = |value: f32, fallback: f32| {
            if value.is_finite() && value > 0.0 {
                value
            } else {
                fallback
            }
        };
        Self {
            free_radius_m: positive(self.free_radius_m, base.free_radius_m),
            online_radius_m: positive(self.online_radius_m, base.online_radius_m),
            move_speed_m_s: positive(self.move_speed_m_s, base.move_speed_m_s),
            fast_mult: positive(self.fast_mult, base.fast_mult),
            look_rad_per_px: positive(self.look_rad_per_px, base.look_rad_per_px),
            pitch_limit_rad: positive(self.pitch_limit_rad, base.pitch_limit_rad),
            dolly_per_notch_m: positive(self.dolly_per_notch_m, base.dolly_per_notch_m),
            fov_min_deg: positive(self.fov_min_deg, base.fov_min_deg),
            fov_max_deg: positive(self.fov_max_deg, base.fov_max_deg).max(self.fov_min_deg),
            roll_max_deg: positive(self.roll_max_deg, base.roll_max_deg),
            exposure_min_ev: if self.exposure_min_ev.is_finite() {
                self.exposure_min_ev
            } else {
                base.exposure_min_ev
            },
            exposure_max_ev: if self.exposure_max_ev.is_finite() {
                self.exposure_max_ev
            } else {
                base.exposure_max_ev
            },
            focus_min_m: positive(self.focus_min_m, base.focus_min_m),
            focus_max_m: positive(self.focus_max_m, base.focus_max_m),
            aperture_min_f: positive(self.aperture_min_f, base.aperture_min_f),
            aperture_max_f: positive(self.aperture_max_f, base.aperture_max_f),
        }
    }

    /// Leash radius for `roam`.
    #[must_use]
    pub fn radius_for(self, roam: Roam) -> f32 {
        match roam {
            Roam::Free => self.free_radius_m,
            Roam::Online => self.online_radius_m,
        }
    }
}

/// The photo eye. `offset` is the eye minus the anchor. Yaw 0 faces +Z.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhotoCamera {
    /// Yaw, radians. Zero faces +Z.
    pub yaw: f32,
    /// Pitch, radians. Positive looks up.
    pub pitch: f32,
    /// Eye minus anchor, metres.
    pub offset: [f32; 3],
    /// Field of view, degrees.
    pub fov_deg: f32,
    /// Roll, degrees.
    pub roll_deg: f32,
    /// Exposure compensation, EV.
    pub exposure_ev: f32,
    /// Focus distance, metres.
    pub focus_m: f32,
    /// Aperture, f-stops.
    pub aperture_f: f32,
    /// Index into [`GRADES`].
    pub grade: usize,
}

impl Default for PhotoCamera {
    fn default() -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            offset: [0.0, 0.0, 0.0],
            fov_deg: FOV_DEFAULT_DEG,
            roll_deg: 0.0,
            exposure_ev: 0.0,
            focus_m: FOCUS_DEFAULT_M,
            aperture_f: APERTURE_DEFAULT_F,
            grade: 0,
        }
    }
}

/// Look direction for `yaw` / `pitch`. Yaw 0 faces +Z.
#[must_use]
pub fn view_dir(yaw: f32, pitch: f32) -> [f32; 3] {
    if !yaw.is_finite() || !pitch.is_finite() {
        return [0.0, 0.0, 1.0];
    }
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    [cp * sy, sp, cp * cy]
}

/// Rotation whose local −Z axis equals `dir`, then rolled about that axis.
///
/// The leash math keeps the source yaw (0 faces +Z). Regia cameras look down −Z,
/// so the transform converts at the write.
#[must_use]
pub fn look_rotation(dir: [f32; 3], roll_rad: f32) -> Quat {
    let mut look = Vec3::from_array(dir);
    if !look.is_finite() || look.length_squared() <= 1.0e-8 {
        look = Vec3::Z;
    } else {
        look = look.normalize();
    }
    let roll = if roll_rad.is_finite() { roll_rad } else { 0.0 };
    Quat::from_rotation_arc(Vec3::NEG_Z, look) * Quat::from_rotation_z(roll)
}

/// Pull `offset` inside `radius` metres. A non-finite offset or radius recentres.
#[must_use]
pub fn clamp_offset_within(offset: [f32; 3], radius: f32) -> [f32; 3] {
    if !offset.iter().all(|v| v.is_finite()) || !radius.is_finite() || radius <= 0.0 {
        return [0.0, 0.0, 0.0];
    }
    let len = (offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2]).sqrt();
    if len <= radius || len < 1.0e-6 {
        return offset;
    }
    let keep = radius / len;
    [offset[0] * keep, offset[1] * keep, offset[2] * keep]
}

/// Pull `offset` inside [`FREE_RADIUS_M`].
#[must_use]
pub fn clamp_offset(offset: [f32; 3]) -> [f32; 3] {
    clamp_offset_within(offset, FREE_RADIUS_M)
}

/// One frame of held movement keys. Both of a pair, or neither, is zero.
#[must_use]
pub fn move_from_held(
    fwd_pos: bool,
    fwd_neg: bool,
    strafe_pos: bool,
    strafe_neg: bool,
    up_pos: bool,
    up_neg: bool,
    fast: bool,
) -> PhotoMove {
    let axis = |pos: bool, neg: bool| match (pos, neg) {
        (true, false) => 1.0,
        (false, true) => -1.0,
        _ => 0.0,
    };
    PhotoMove {
        fwd: axis(fwd_pos, fwd_neg),
        strafe: axis(strafe_pos, strafe_neg),
        up: axis(up_pos, up_neg),
        fast,
    }
}

/// Keys and rates for one frame. The caller fills this; the module never reads a device.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PhotoKeys {
    /// Toggle went down this frame.
    pub toggle: bool,
    /// Capture went down this frame.
    pub capture: bool,
    /// Exit went down this frame.
    pub escape: bool,
    /// Move rates.
    pub mv: PhotoMove,
    /// Look pixels, x.
    pub look_dx: f32,
    /// Look pixels, y. Positive looks up.
    pub look_dy: f32,
    /// Wheel notches. Positive dollies toward the look point.
    pub dolly: i32,
    /// Frame step, seconds.
    pub dt: f32,
}

/// What the host should do after [`PhotoState::drive`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhotoOutcome {
    /// Photo mode is open.
    pub active: bool,
    /// The HUD stays hidden while photo mode is open.
    pub hud_hidden: bool,
    /// Always false.
    pub pause_sim: bool,
    /// While active the host must not let camera keys drive the sim.
    pub neutral_drive: bool,
    /// Write one still this frame. False on the frame photo mode closes.
    pub capture: bool,
    /// Photo mode closed this frame.
    pub exited: bool,
    /// Photo mode opened this frame.
    pub opened: bool,
    /// Why an open was refused.
    pub denied: Option<DenyReason>,
    /// Eye, world metres.
    pub eye: [f32; 3],
    /// Look-at point, world metres.
    pub target: [f32; 3],
    /// Field of view, degrees.
    pub fov_deg: f32,
    /// Leash in force, metres.
    pub radius_m: f32,
}

impl PhotoCamera {
    /// Aim from mouse pixels using the default look rate and pitch limit.
    pub fn look(&mut self, dx_px: f32, dy_px: f32) {
        self.look_within(dx_px, dy_px, LOOK_RAD_PER_PX, PITCH_LIMIT_RAD);
    }

    /// Aim from mouse pixels.
    pub fn look_within(&mut self, dx_px: f32, dy_px: f32, look_rad_per_px: f32, pitch_limit: f32) {
        let dx = finite_or(dx_px, 0.0);
        let dy = finite_or(dy_px, 0.0);
        let rate = finite_or(look_rad_per_px, 0.0);
        let limit = finite_or(pitch_limit, PITCH_LIMIT_RAD).abs();
        self.yaw += dx * rate;
        self.pitch = (self.pitch + dy * rate).clamp(-limit, limit);
    }

    /// Move the eye for one frame, then clamp to [`FREE_RADIUS_M`].
    pub fn step_photo(&mut self, mv: PhotoMove, dt: f32) {
        self.step_within(mv, dt, MOVE_SPEED_M_S, FAST_MULT, FREE_RADIUS_M);
    }

    /// Move the eye and clamp to `radius`.
    pub fn step_within(&mut self, mv: PhotoMove, dt: f32, speed_m_s: f32, fast_mult: f32, radius: f32) {
        let dt = finite_or(dt, 0.0).clamp(0.0, 1.0);
        let speed = finite_or(speed_m_s, 0.0).max(0.0)
            * if mv.fast {
                finite_or(fast_mult, 1.0).max(0.0)
            } else {
                1.0
            }
            * dt;
        let dir = view_dir(self.yaw, self.pitch);
        let right = {
            let len = (dir[0] * dir[0] + dir[2] * dir[2]).sqrt();
            if len > 1.0e-4 {
                [dir[2] / len, 0.0, -dir[0] / len]
            } else {
                [1.0, 0.0, 0.0]
            }
        };
        let fwd = clamp_rate(mv.fwd) * speed;
        let strafe = clamp_rate(mv.strafe) * speed;
        let up = clamp_rate(mv.up) * speed;
        self.offset = clamp_offset_within(
            [
                self.offset[0] + dir[0] * fwd + right[0] * strafe,
                self.offset[1] + dir[1] * fwd + up,
                self.offset[2] + dir[2] * fwd + right[2] * strafe,
            ],
            radius,
        );
    }

    /// Wheel notches, clamped to [`FREE_RADIUS_M`].
    pub fn dolly(&mut self, notches: i32) {
        self.dolly_within(notches, DOLLY_PER_NOTCH_M, FREE_RADIUS_M);
    }

    /// Wheel notches, clamped to `radius`.
    pub fn dolly_within(&mut self, notches: i32, metres_per_notch: f32, radius: f32) {
        let dir = view_dir(self.yaw, self.pitch);
        let push = (notches as f32) * finite_or(metres_per_notch, 0.0);
        self.offset = clamp_offset_within(
            [
                self.offset[0] + dir[0] * push,
                self.offset[1] + dir[1] * push,
                self.offset[2] + dir[2] * push,
            ],
            radius,
        );
    }

    /// Add degrees of field of view and clamp to the default range.
    pub fn nudge_fov(&mut self, delta_deg: f32) {
        let delta = finite_or(delta_deg, 0.0);
        self.fov_deg = (self.fov_deg + delta).clamp(FOV_MIN_DEG, FOV_MAX_DEG);
    }

    /// Add degrees of roll and clamp to ±[`ROLL_MAX_DEG`].
    pub fn nudge_roll(&mut self, delta_deg: f32) {
        let delta = finite_or(delta_deg, 0.0);
        self.roll_deg = (self.roll_deg + delta).clamp(-ROLL_MAX_DEG, ROLL_MAX_DEG);
    }

    /// Add exposure compensation and clamp to ±[`EXPOSURE_MAX_EV`].
    pub fn nudge_exposure(&mut self, delta_ev: f32) {
        let delta = finite_or(delta_ev, 0.0);
        self.exposure_ev = (self.exposure_ev + delta).clamp(EXPOSURE_MIN_EV, EXPOSURE_MAX_EV);
    }

    /// Multiply focus distance. The factor is clamped to 0.25..=4.
    pub fn nudge_focus(&mut self, factor: f32) {
        let factor = finite_or(factor, 1.0).clamp(0.25, 4.0);
        self.focus_m = (self.focus_m * factor).clamp(FOCUS_MIN_M, FOCUS_MAX_M);
    }

    /// Multiply aperture by `2^stops`. `stops` is clamped to ±3.
    pub fn nudge_aperture(&mut self, stops: f32) {
        let stops = finite_or(stops, 0.0).clamp(-3.0, 3.0);
        self.aperture_f = (self.aperture_f * 2.0f32.powf(stops)).clamp(APERTURE_MIN_F, APERTURE_MAX_F);
    }

    /// Step the grade preset, wrapping.
    pub fn cycle_grade(&mut self, step: i32) {
        let n = GRADES.len() as i32;
        self.grade = (self.grade as i32 + step).rem_euclid(n) as usize;
    }

    /// The current grade.
    #[must_use]
    pub fn grade(&self) -> Grade {
        GRADES.get(self.grade).copied().unwrap_or(GRADES[0])
    }

    /// Focus and aperture.
    #[must_use]
    pub fn depth_of_field(&self) -> DepthOfField {
        DepthOfField {
            focus_m: self.focus_m,
            aperture_f: self.aperture_f,
        }
    }

    /// The eye in world space.
    #[must_use]
    pub fn eye(&self, anchor: [f32; 3]) -> [f32; 3] {
        [
            anchor[0] + self.offset[0],
            anchor[1] + self.offset[1],
            anchor[2] + self.offset[2],
        ]
    }

    /// The look-at point in world space.
    #[must_use]
    pub fn target(&self, anchor: [f32; 3]) -> [f32; 3] {
        let eye = self.eye(anchor);
        let dir = view_dir(self.yaw, self.pitch);
        [eye[0] + dir[0], eye[1] + dir[1], eye[2] + dir[2]]
    }

    /// Roll in radians.
    #[must_use]
    pub fn roll_rad(&self) -> f32 {
        self.roll_deg.to_radians()
    }
}

/// Photo session. While [`Self::active`] the HUD hides and drive input stays neutral.
#[derive(Clone, Debug, PartialEq)]
pub struct PhotoState {
    /// Photo mode is open.
    pub active: bool,
    /// Leash centre, world metres.
    pub anchor: [f32; 3],
    /// The eye.
    pub camera: PhotoCamera,
    /// Leash in force while active.
    pub roam: Roam,
    /// Settings. Defaults equal the source constants.
    pub limits: PhotoLimits,
    saved: Option<SavedView>,
}

impl Default for PhotoState {
    fn default() -> Self {
        Self {
            active: false,
            anchor: [0.0; 3],
            camera: PhotoCamera::default(),
            roam: Roam::Free,
            limits: PhotoLimits::default(),
            saved: None,
        }
    }
}

impl PhotoState {
    /// Open around `anchor`. Stores `saved` so [`Self::exit`] restores it.
    ///
    /// # Errors
    /// [`DenyReason::Competitive`] when the match is competitive.
    pub fn try_enter(
        &mut self,
        access: MatchAccess,
        saved: SavedView,
        anchor: [f32; 3],
    ) -> Result<(), DenyReason> {
        if !allowed(access) {
            return Err(DenyReason::Competitive);
        }
        self.active = true;
        self.anchor = if anchor.iter().all(|v| v.is_finite()) {
            anchor
        } else {
            [0.0; 3]
        };
        self.camera = PhotoCamera::default();
        self.saved = Some(saved);
        self.roam = roam_of(access);
        self.camera.offset = clamp_offset_within(self.camera.offset, self.radius());
        Ok(())
    }

    /// Metres the eye may sit from [`Self::anchor`] right now.
    #[must_use]
    pub fn radius(&self) -> f32 {
        self.limits.radius_for(self.roam)
    }

    /// One frame. Toggle opens (when [`allowed`]) and closes. Exit closes.
    #[must_use]
    pub fn drive(
        &mut self,
        access: MatchAccess,
        saved: SavedView,
        anchor: [f32; 3],
        keys: PhotoKeys,
    ) -> PhotoOutcome {
        let mut exited = false;
        let mut opened = false;
        let mut denied = None;
        if self.active && (keys.toggle || keys.escape) {
            self.exit();
            exited = true;
        } else if !self.active && keys.toggle {
            match self.try_enter(access, saved, anchor) {
                Ok(()) => opened = true,
                Err(reason) => denied = Some(reason),
            }
        }
        if self.active {
            self.roam = roam_of(access);
            if anchor.iter().all(|v| v.is_finite()) {
                self.anchor = anchor;
            }
            self.camera.look_within(
                keys.look_dx,
                keys.look_dy,
                self.limits.look_rad_per_px,
                self.limits.pitch_limit_rad,
            );
            self.camera.step_within(
                keys.mv,
                keys.dt,
                self.limits.move_speed_m_s,
                self.limits.fast_mult,
                self.radius(),
            );
            if keys.dolly != 0 {
                self.camera
                    .dolly_within(keys.dolly, self.limits.dolly_per_notch_m, self.radius());
            }
            self.camera.offset = clamp_offset_within(self.camera.offset, self.radius());
        }
        PhotoOutcome {
            active: self.active,
            hud_hidden: self.hud_hidden(),
            pause_sim: pauses_sim(access),
            neutral_drive: self.active,
            capture: self.active && keys.capture && !exited,
            exited,
            opened,
            denied,
            eye: self.camera.eye(self.anchor),
            target: self.camera.target(self.anchor),
            fov_deg: self.camera.fov_deg,
            radius_m: self.radius(),
        }
    }

    /// Close photo mode and hand back the view stored at entry.
    pub fn exit(&mut self) -> Option<SavedView> {
        self.active = false;
        self.saved.take()
    }

    /// True while the host must hide the HUD.
    #[must_use]
    pub fn hud_hidden(&self) -> bool {
        self.active
    }

    /// The view stored at entry.
    #[must_use]
    pub fn saved_view(&self) -> Option<SavedView> {
        self.saved
    }
}

/// Frame size for a capture. `hires` multiplies by [`CAPTURE_SCALE_HIRES`], then
/// the long side is fit under [`CAPTURE_MAX_DIM`].
#[must_use]
pub fn capture_size(native_w: u32, native_h: u32, hires: bool) -> (u32, u32) {
    let (mut w, mut h) = if native_w == 0 || native_h == 0 {
        FALLBACK_SIZE
    } else {
        (native_w.max(1), native_h.max(1))
    };
    if hires {
        w = w.saturating_mul(CAPTURE_SCALE_HIRES);
        h = h.saturating_mul(CAPTURE_SCALE_HIRES);
    }
    let long = w.max(h);
    if long > CAPTURE_MAX_DIM {
        let keep = f64::from(CAPTURE_MAX_DIM) / f64::from(long);
        w = scale_dim(w, keep);
        h = scale_dim(h, keep);
    }
    (w, h)
}

fn scale_dim(side: u32, keep: f64) -> u32 {
    let scaled = f64::from(side) * keep;
    if !scaled.is_finite() || scaled < 1.0 {
        return 1;
    }
    let whole = scaled.floor();
    if whole >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        // `whole` is finite, at least 1, and below `u32::MAX`.
        whole as u32
    }
}

/// File-safe token: lowercase ASCII letters and digits stay, anything else becomes `_`.
/// Empty input becomes `empty_fallback`. Capped at 32 chars, then trimmed of `_`.
#[must_use]
pub fn sanitize_token(raw: &str, empty_fallback: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
        if out.len() >= 32 {
            break;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        empty_fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Map token. Empty input becomes `map`.
#[must_use]
pub fn sanitize_map_id(map: &str) -> String {
    sanitize_token(map, "map")
}

/// Game token. Empty input becomes `game`.
#[must_use]
pub fn sanitize_game_id(game: &str) -> String {
    sanitize_token(game, "game")
}

/// `<game>-<map>-t<tick>-<n>.png`. The tick is zero-padded to at least six digits.
/// Hi-res does not change the name.
#[must_use]
pub fn capture_name(game: &str, map: &str, tick: u64, n: u32) -> String {
    let game = sanitize_game_id(game);
    let map = sanitize_map_id(map);
    let n = n.max(1);
    format!("{game}-{map}-t{tick:06}-{n}.png")
}

/// First free capture name. `n` starts at 1. `exists` is the file probe.
#[must_use]
pub fn next_capture_name(game: &str, map: &str, tick: u64, exists: &dyn Fn(&str) -> bool) -> String {
    let mut last = capture_name(game, map, tick, 1);
    for n in 1..=CAPTURE_NAME_PROBE_CAP {
        let name = capture_name(game, map, tick, n);
        if !exists(&name) {
            return name;
        }
        last = name;
    }
    last
}

/// Length of an offset.
#[must_use]
pub fn offset_len(offset: [f32; 3]) -> f32 {
    (offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2]).sqrt()
}

/// Why a still could not be written.
#[derive(Debug)]
pub enum CaptureError {
    /// No directory was configured.
    NoDirectory,
    /// The name probe found only existing files.
    NameExhausted,
    /// Creating or writing the file failed.
    Io(std::io::Error),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDirectory => f.write_str("photo capture has no pictures directory"),
            Self::NameExhausted => f.write_str("photo capture names are exhausted"),
            Self::Io(err) => write!(f, "photo capture write failed: {err}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Choose a free name in `dir` and write a deterministic RGBA PNG of `width` by `height`.
///
/// # Errors
/// [`CaptureError::NameExhausted`] or [`CaptureError::Io`].
#[cfg(feature = "client")]
pub fn write_capture_png(
    dir: &Path,
    game: &str,
    map: &str,
    tick: u64,
    width: u32,
    height: u32,
) -> Result<(String, PathBuf), CaptureError> {
    std::fs::create_dir_all(dir).map_err(CaptureError::Io)?;
    let width = width.max(1);
    let height = height.max(1);
    for n in 1..=CAPTURE_NAME_PROBE_CAP {
        let name = capture_name(game, map, tick, n);
        let dest = dir.join(&name);
        if dest.exists() {
            continue;
        }
        let tmp = dir.join(format!(".{name}.tmp"));
        match write_png_file(&tmp, width, height) {
            Ok(()) => match std::fs::rename(&tmp, &dest) {
                Ok(()) => return Ok((name, dest)),
                Err(err) => {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(CaptureError::Io(err));
                }
            },
            Err(err) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(CaptureError::Io(err));
            }
        }
    }
    Err(CaptureError::NameExhausted)
}

#[cfg(feature = "client")]
fn write_png_file(path: &Path, width: u32, height: u32) -> std::io::Result<()> {
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .and_then(|count| count.checked_mul(4))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "capture is too large"))?;
    let mut rgba = vec![0u8; pixels];
    let mut i = 0;
    while i + 3 < rgba.len() {
        rgba[i] = 32;
        rgba[i + 1] = 32;
        rgba[i + 2] = 32;
        rgba[i + 3] = 255;
        i += 4;
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        writer
            .write_image_data(&rgba)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    }
    let mut file = std::fs::File::create(path)?;
    std::io::Write::write_all(&mut file, &bytes)?;
    file.sync_all()?;
    Ok(())
}

/// HUD-hide flag the runtime UI honors while photo mode is open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhotoHud {
    /// True while HUD nodes should be `Display::None`.
    pub hidden: bool,
}

/// Marks the camera entity photo mode writes.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhotoView;

/// Photo mode session resource.
#[derive(Resource, Debug, Clone)]
pub struct PhotoMode {
    /// Session logic.
    pub state: PhotoState,
    /// Access used by the frame driver.
    pub access: MatchAccess,
    /// Sim tick captured into file names. Advanced in `FixedUpdate`.
    pub tick: u64,
    /// Game token for capture names.
    pub game: String,
    /// Map token for capture names.
    pub map: String,
    /// Pictures directory. `None` until the plugin or a caller sets it.
    pub pictures: Option<PathBuf>,
    /// Native frame width. Zero uses [`FALLBACK_SIZE`].
    pub frame_w: u32,
    /// Native frame height. Zero uses [`FALLBACK_SIZE`].
    pub frame_h: u32,
    /// Double the frame before the max-dimension fit.
    pub hires: bool,
    /// Anchor entity. Its translation replaces [`PhotoState::anchor`] each frame.
    pub follow: Option<Entity>,
    /// Deterministic frame step. `None` uses `Time::delta_secs` (zero while paused).
    pub frame_dt: Option<f32>,
    /// View handed to [`PhotoState::try_enter`] by the driver.
    pub entry_saved: SavedView,
    /// HUD flag mirrored from the session.
    pub hud: PhotoHud,
    prev_toggle: bool,
    prev_capture: bool,
    prev_exit: bool,
    just_opened: bool,
    just_exited: bool,
    capture_requested: bool,
    /// HUD display codes displaced while photo mode is open (the `client` HUD system only).
    #[cfg(feature = "client")]
    hud_saved: Vec<(Entity, u8)>,
    /// Camera fields displaced while photo mode is open, restored on exit (the `client` camera
    /// system only).
    #[cfg(feature = "client")]
    view_saved: Vec<ViewSnap>,
}

/// Gameplay camera fields photo mode writes and then puts back.
#[cfg(feature = "client")]
#[derive(Clone, Copy, Debug)]
struct ViewSnap {
    entity: u64,
    fov_y: f32,
    exposure_ev: Option<f32>,
    had_lens: bool,
    focal_distance: f32,
    aperture_f_stops: f32,
    dof_gaussian: bool,
}

impl Default for PhotoMode {
    fn default() -> Self {
        Self {
            state: PhotoState::default(),
            access: MatchAccess::default(),
            tick: 0,
            game: String::new(),
            map: String::new(),
            pictures: None,
            frame_w: 0,
            frame_h: 0,
            hires: true,
            follow: None,
            frame_dt: None,
            entry_saved: SavedView {
                fov_deg: FOV_DEFAULT_DEG,
                hud_visible: true,
            },
            hud: PhotoHud::default(),
            prev_toggle: false,
            prev_capture: false,
            prev_exit: false,
            just_opened: false,
            just_exited: false,
            capture_requested: false,
            #[cfg(feature = "client")]
            hud_saved: Vec::new(),
            #[cfg(feature = "client")]
            view_saved: Vec::new(),
        }
    }
}

impl PhotoMode {
    /// JSON status for `photo.status`.
    #[must_use]
    pub fn status_json(&self) -> serde_json::Value {
        let camera = &self.state.camera;
        let grade = camera.grade();
        serde_json::json!({
            "active": self.state.active,
            "hud_hidden": self.state.hud_hidden(),
            "allowed": allowed(self.access),
            "roam": match self.state.roam {
                Roam::Free => "free",
                Roam::Online => "online",
            },
            "radius_m": self.state.radius(),
            "eye": self.state.camera.eye(self.state.anchor),
            "target": self.state.camera.target(self.state.anchor),
            "anchor": self.state.anchor,
            "yaw": camera.yaw,
            "pitch": camera.pitch,
            "fov_deg": camera.fov_deg,
            "roll_deg": camera.roll_deg,
            "exposure_ev": camera.exposure_ev,
            "focus_m": camera.focus_m,
            "aperture_f": camera.aperture_f,
            "grade": grade.name,
            "tick": self.tick,
            "game": self.game,
            "map": self.map,
            "hires": self.hires,
            "pause_sim": false,
        })
    }

    /// A session with a fixed frame step, a known frame size, and no pictures folder.
    ///
    /// [`PhotoModePlugin`] fills the platform pictures directory when it inserts the
    /// resource itself. This constructor is for a caller that already owns the resource.
    #[must_use]
    pub fn fixed_frame(access: MatchAccess, dt_secs: f32, width: u32, height: u32, hires: bool) -> Self {
        Self {
            access,
            frame_dt: Some(dt_secs),
            frame_w: width,
            frame_h: height,
            hires,
            pictures: None,
            ..Self::default()
        }
    }

    /// Record that photo mode opened this frame.
    ///
    /// PostUpdate hides the HUD and snapshots the camera, then clears the edge.
    /// The agent sets it because those fields stay private to this module.
    pub fn mark_opened(&mut self) {
        self.just_opened = true;
        self.just_exited = false;
    }
}

/// Drives [`PhotoMode`] and, on a client build, the HUD and the photo camera.
pub struct PhotoModePlugin;

impl Plugin for PhotoModePlugin {
    fn build(&self, app: &mut App) {
        if app.world().get_resource::<PhotoMode>().is_none() {
            let mode = PhotoMode {
                pictures: regia_platform::paths::pictures_dir(),
                ..PhotoMode::default()
            };
            app.insert_resource(mode);
        }
        app.add_systems(FixedUpdate, advance_photo_tick);
        app.add_systems(Update, drive_photo_mode);
        #[cfg(feature = "client")]
        {
            app.add_systems(
                PostUpdate,
                (apply_photo_hud, apply_photo_camera, consume_photo_edges)
                    .chain()
                    .before(PostUpdateSet::TransformPropagate),
            );
        }
    }
}

fn advance_photo_tick(mut mode: ResMut<PhotoMode>) {
    mode.tick = mode.tick.saturating_add(1);
}

#[tracing::instrument(skip_all, target = "regia_framework", name = "photo_drive")]
fn drive_photo_mode(
    time: Res<Time>,
    actions: Option<Res<Actions>>,
    injection: Option<Res<InputInjection>>,
    mut mode: ResMut<PhotoMode>,
    transforms: Query<&Transform, Without<PhotoView>>,
) {
    if let Some(entity) = mode.follow
        && let Ok(transform) = transforms.get(entity)
    {
        let t = transform.translation;
        if t.is_finite() {
            mode.state.anchor = [t.x, t.y, t.z];
        }
    }
    let dt = mode.frame_dt.unwrap_or_else(|| time.delta_secs());
    let frame = time.frame_count();
    let keys = keys_from(actions.as_deref(), injection.as_deref(), frame, dt, &mut mode);
    let saved = mode.entry_saved;
    let anchor = mode.state.anchor;
    let access = mode.access;
    let outcome = mode.state.drive(access, saved, anchor, keys);
    // Keep an edge the agent set earlier this frame. PostUpdate consumes it.
    // An exit in the same frame wins, so the HUD is not hidden and then left hidden.
    if outcome.exited {
        mode.just_opened = false;
        mode.just_exited = true;
    } else if outcome.opened {
        mode.just_opened = true;
        mode.just_exited = false;
    }
    mode.hud.hidden = outcome.hud_hidden;
    if outcome.capture {
        mode.capture_requested = true;
    }
    if outcome.capture
        && let Some(dir) = mode.pictures.clone()
    {
        let (w, h) = capture_size(mode.frame_w, mode.frame_h, mode.hires);
        #[cfg(feature = "client")]
        {
            if let Err(err) = write_capture_png(&dir, &mode.game, &mode.map, mode.tick, w, h) {
                tracing::warn!(target: "regia_framework", "photo capture: {err}");
            }
        }
        #[cfg(not(feature = "client"))]
        {
            let _ = (dir, w, h);
        }
        mode.capture_requested = false;
    }
}

fn keys_from(
    actions: Option<&Actions>,
    injection: Option<&InputInjection>,
    frame: u64,
    dt: f32,
    mode: &mut PhotoMode,
) -> PhotoKeys {
    let level = |name: &str| -> bool {
        if let Some(actions) = actions
            && actions.player(0).states.contains_key(name)
        {
            return actions.pressed(name);
        }
        injection
            .and_then(|injection| injection.value_for(name, frame))
            .is_some_and(|value| value.length_squared() > 0.0)
    };
    let edge = |now: bool, prev: &mut bool| {
        let fired = now && !*prev;
        *prev = now;
        fired
    };
    let move_axis = axis2(actions, injection, MOVE_ACTION, frame);
    let look = axis2(actions, injection, LOOK_ACTION, frame);
    let up = axis1(actions, injection, UP_ACTION, frame);
    let dolly = axis1(actions, injection, DOLLY_ACTION, frame);
    PhotoKeys {
        toggle: edge(level(TOGGLE_ACTION), &mut mode.prev_toggle),
        capture: edge(level(CAPTURE_ACTION), &mut mode.prev_capture),
        escape: edge(level(EXIT_ACTION), &mut mode.prev_exit),
        mv: PhotoMove {
            fwd: move_axis[1],
            strafe: move_axis[0],
            up,
            fast: level(FAST_ACTION),
        },
        look_dx: look[0],
        look_dy: look[1],
        dolly: dolly.round() as i32,
        dt,
    }
}

fn axis2(actions: Option<&Actions>, injection: Option<&InputInjection>, name: &str, frame: u64) -> [f32; 2] {
    if let Some(actions) = actions
        && actions.player(0).states.contains_key(name)
    {
        let value = actions.axis2d(name);
        return [value.x, value.y];
    }
    injection
        .and_then(|injection| injection.value_for(name, frame))
        .map(|value| [value.x, value.y])
        .unwrap_or([0.0, 0.0])
}

fn axis1(actions: Option<&Actions>, injection: Option<&InputInjection>, name: &str, frame: u64) -> f32 {
    if let Some(actions) = actions
        && actions.player(0).states.contains_key(name)
    {
        return actions.axis1d(name);
    }
    injection
        .and_then(|injection| injection.value_for(name, frame))
        .map(|value| value.x)
        .unwrap_or(0.0)
}

#[cfg(feature = "client")]
fn apply_photo_hud(
    mut mode: ResMut<PhotoMode>,
    mut nodes: Query<(
        Entity,
        &crate::sequencer::cutscene::HudLayer,
        Option<&mut regia_ui::NodeStyle>,
    )>,
) {
    if mode.just_opened {
        mode.hud_saved.clear();
        for (entity, _layer, style) in &mut nodes {
            let code = style
                .as_ref()
                .map(|style| display_code(style.0.display))
                .unwrap_or(4);
            mode.hud_saved.push((entity, code));
        }
        mode.hud_saved.sort_by_key(|(entity, _)| entity.to_bits());
        for (entity, _) in mode.hud_saved.clone() {
            if let Some((_, _, Some(mut style))) = nodes.iter_mut().find(|(id, _, _)| *id == entity) {
                style.0.display = regia_ui::Display::None;
            }
        }
    } else if mode.just_exited {
        let saved = std::mem::take(&mut mode.hud_saved);
        for (entity, code) in saved {
            if let Some((_, _, Some(mut style))) = nodes.iter_mut().find(|(id, _, _)| *id == entity) {
                style.0.display = display_from_code(code);
            }
        }
    }
}

#[cfg(feature = "client")]
fn display_code(display: regia_ui::Display) -> u8 {
    match display {
        regia_ui::Display::Flex => 0,
        regia_ui::Display::Grid => 1,
        regia_ui::Display::Block => 2,
        regia_ui::Display::None => 3,
    }
}

#[cfg(feature = "client")]
fn display_from_code(code: u8) -> regia_ui::Display {
    match code {
        1 => regia_ui::Display::Grid,
        2 => regia_ui::Display::Block,
        3 => regia_ui::Display::None,
        _ => regia_ui::Display::Flex,
    }
}

#[cfg(feature = "client")]
#[tracing::instrument(skip_all, target = "regia_framework", name = "photo_camera")]
fn apply_photo_camera(
    mut mode: ResMut<PhotoMode>,
    mut views: Query<
        (
            Entity,
            &mut Transform,
            &mut regia_render::Camera,
            Option<&mut regia_render::PhysicalCamera>,
        ),
        With<PhotoView>,
    >,
) {
    if mode.just_exited {
        let saved = std::mem::take(&mut mode.view_saved);
        for (entity, _transform, mut camera, lens) in &mut views {
            if let Some(snap) = saved.iter().find(|snap| snap.entity == entity.to_bits()) {
                camera.fov_y = snap.fov_y;
                camera.exposure_ev = snap.exposure_ev;
                if let Some(mut lens) = lens
                    && snap.had_lens
                {
                    lens.focal_distance = snap.focal_distance;
                    lens.aperture_f_stops = snap.aperture_f_stops;
                    lens.depth_of_field = if snap.dof_gaussian {
                        Some(regia_render::DepthOfFieldMode::Gaussian)
                    } else {
                        None
                    };
                }
            }
        }
        return;
    }
    if !mode.state.active {
        return;
    }
    if mode.just_opened {
        mode.view_saved.clear();
    }
    let eye = Vec3::from_array(mode.state.camera.eye(mode.state.anchor));
    let rotation = look_rotation(
        view_dir(mode.state.camera.yaw, mode.state.camera.pitch),
        mode.state.camera.roll_rad(),
    );
    let fov = mode.state.camera.fov_deg.to_radians();
    let bias = mode.state.camera.exposure_ev;
    let focus = mode.state.camera.focus_m;
    let aperture = mode.state.camera.aperture_f;
    let remember = mode.just_opened || mode.view_saved.is_empty();
    for (entity, mut transform, mut camera, lens) in &mut views {
        if remember && !mode.view_saved.iter().any(|snap| snap.entity == entity.to_bits()) {
            let (had_lens, focal_distance, aperture_f_stops, dof_gaussian) =
                lens.as_ref().map_or((false, 0.0, 0.0, false), |lens| {
                    (
                        true,
                        lens.focal_distance,
                        lens.aperture_f_stops,
                        matches!(
                            lens.depth_of_field,
                            Some(regia_render::DepthOfFieldMode::Gaussian)
                        ),
                    )
                });
            mode.view_saved.push(ViewSnap {
                entity: entity.to_bits(),
                fov_y: camera.fov_y,
                exposure_ev: camera.exposure_ev,
                had_lens,
                focal_distance,
                aperture_f_stops,
                dof_gaussian,
            });
        }
        transform.translation = eye;
        transform.rotation = rotation;
        camera.fov_y = fov;
        if let Some(snap) = mode
            .view_saved
            .iter()
            .find(|snap| snap.entity == entity.to_bits())
            && let Some(base) = snap.exposure_ev
        {
            camera.exposure_ev = Some(base + bias);
        }
        if let Some(mut lens) = lens {
            lens.focal_distance = focus;
            lens.aperture_f_stops = aperture;
            lens.depth_of_field = Some(regia_render::DepthOfFieldMode::Gaussian);
            lens.focal_length = None;
        }
    }
}

#[cfg(feature = "client")]
fn consume_photo_edges(mut mode: ResMut<PhotoMode>) {
    mode.just_opened = false;
    mode.just_exited = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solo_live() -> MatchAccess {
        MatchAccess::default()
    }

    fn online_live() -> MatchAccess {
        MatchAccess {
            networked: true,
            ..MatchAccess::default()
        }
    }

    fn saved_view() -> SavedView {
        SavedView {
            fov_deg: 90.0,
            hud_visible: true,
        }
    }

    fn fly_fast() -> PhotoKeys {
        PhotoKeys {
            mv: PhotoMove {
                fwd: 1.0,
                fast: true,
                ..PhotoMove::default()
            },
            dt: 30.0,
            ..PhotoKeys::default()
        }
    }

    #[test]
    fn solo_opens_anytime_but_competitive_never_does() {
        assert!(allowed(solo_live()));
        assert!(allowed(MatchAccess {
            paused: true,
            ..MatchAccess::default()
        }));
        assert!(allowed(online_live()));
        assert!(allowed(MatchAccess {
            networked: true,
            paused: true,
            ..MatchAccess::default()
        }));
        assert!(!allowed(MatchAccess {
            networked: true,
            competitive: true,
            paused: true,
            ..MatchAccess::default()
        }));
        assert!(!allowed(MatchAccess {
            networked: true,
            competitive: true,
            ..MatchAccess::default()
        }));
        assert!(!allowed(MatchAccess {
            competitive: true,
            paused: true,
            ..MatchAccess::default()
        }));
        assert_eq!(roam_of(solo_live()), Roam::Free);
        assert_eq!(roam_of(online_live()), Roam::Online);
        assert_eq!(
            roam_of(MatchAccess {
                networked: true,
                training: true,
                ..MatchAccess::default()
            }),
            Roam::Free
        );
        assert_eq!(
            roam_of(MatchAccess {
                networked: true,
                dead: true,
                ..MatchAccess::default()
            }),
            Roam::Free
        );
        assert_eq!(
            roam_of(MatchAccess {
                networked: true,
                spectating: true,
                ..MatchAccess::default()
            }),
            Roam::Free
        );
    }

    #[test]
    fn entering_and_leaving_restores_the_camera_and_hud() {
        let mut state = PhotoState::default();
        let saved = SavedView {
            fov_deg: 90.0,
            hud_visible: true,
        };
        state
            .try_enter(solo_live(), saved, [10.0, 2.0, 30.0])
            .expect("solo may open");
        assert!(state.active);
        assert!(state.hud_hidden(), "the HUD hides while photo mode is open");
        state.camera.look(120.0, -40.0);
        state.camera.step_photo(
            PhotoMove {
                fwd: 1.0,
                ..PhotoMove::default()
            },
            1.0,
        );
        state.camera.nudge_fov(-20.0);
        let back = state.exit().expect("entry stored a view");
        assert_eq!(back, saved, "exit hands back the entry view unchanged");
        assert!(!state.active);
        assert!(!state.hud_hidden());
        assert!(
            PhotoState::default().exit().is_none(),
            "no entry, nothing to restore"
        );
    }

    #[test]
    fn entry_refusals_name_the_reason() {
        let mut state = PhotoState::default();
        let saved = SavedView {
            fov_deg: 90.0,
            hud_visible: true,
        };
        assert_eq!(
            state.try_enter(
                MatchAccess {
                    networked: true,
                    competitive: true,
                    paused: true,
                    ..MatchAccess::default()
                },
                saved,
                [0.0; 3]
            ),
            Err(DenyReason::Competitive)
        );
        assert!(!state.active);
        assert_eq!(state.try_enter(online_live(), saved, [4.0, 1.0, 2.0]), Ok(()));
        assert!(state.active);
        assert_eq!(state.roam, Roam::Online);
        assert!((state.radius() - ONLINE_RADIUS_M).abs() < 1.0e-4);
        assert!(!pauses_sim(online_live()));
    }

    #[test]
    fn the_radius_clamp_holds_the_eye_near_the_player() {
        assert_eq!(clamp_offset([3.0, 0.0, 4.0]), [3.0, 0.0, 4.0], "inside stays");
        let held = clamp_offset([30.0, 0.0, 40.0]);
        let len = (held[0] * held[0] + held[2] * held[2]).sqrt();
        assert!(
            (len - RADIUS_M).abs() < 1.0e-3,
            "outside pulls back to {RADIUS_M} m: {held:?}"
        );
        assert!(held[0] > 0.0 && held[2] > 0.0, "the direction survives: {held:?}");
        assert_eq!(clamp_offset([0.0; 3]), [0.0; 3]);
        assert_eq!(
            clamp_offset([f32::NAN, 1.0, 2.0]),
            [0.0; 3],
            "a bad offset recentres"
        );
        assert_eq!(clamp_offset([f32::INFINITY, 0.0, 0.0]), [0.0; 3]);
        let mut camera = PhotoCamera::default();
        camera.step_photo(
            PhotoMove {
                fwd: 1.0,
                ..PhotoMove::default()
            },
            60.0,
        );
        let len = offset_len(camera.offset);
        assert!(len <= RADIUS_M + 1.0e-3, "a long drive ends on the shell: {len}");
    }

    #[test]
    fn capture_names_carry_the_tick_and_not_a_scale_suffix() {
        assert_eq!(
            capture_name("ridge", "north", 123, 1),
            "ridge-north-t000123-1.png"
        );
        assert_eq!(
            capture_name("ridge", "north", 123, 1),
            capture_name("ridge", "north", 123, 1)
        );
        assert_eq!(
            capture_name("ridge", "Switchbacks!", 7, 1),
            "ridge-switchbacks-t000007-1.png"
        );
        assert_eq!(capture_name("", "", 7, 1), "game-map-t000007-1.png");
        assert_eq!(
            capture_name("ridge", "north", 1_234_567, 1),
            "ridge-north-t1234567-1.png"
        );
        let none = |_: &str| false;
        assert_eq!(
            next_capture_name("ridge", "north", 123, &none),
            "ridge-north-t000123-1.png"
        );
        let first_taken = |name: &str| name == "ridge-north-t000123-1.png";
        assert_eq!(
            next_capture_name("ridge", "north", 123, &first_taken),
            "ridge-north-t000123-2.png"
        );
    }

    #[test]
    fn capture_sizes_double_then_fit_the_cap() {
        assert_eq!(capture_size(1920, 1080, false), (1920, 1080));
        assert_eq!(capture_size(1920, 1080, true), (3840, 2160));
        assert_eq!(capture_size(0, 0, false), FALLBACK_SIZE);
        let (w, h) = capture_size(5000, 3000, true);
        assert_eq!(w.max(h), CAPTURE_MAX_DIM, "a huge 2x still shrinks to the cap");
        assert!(
            (f64::from(w) / f64::from(h) - 5000.0 / 3000.0).abs() < 0.01,
            "the aspect survives"
        );
    }

    #[test]
    fn grades_cycle_and_settings_clamp() {
        let mut camera = PhotoCamera::default();
        assert_eq!(camera.grade().name, "NEUTRAL");
        camera.cycle_grade(1);
        assert_eq!(camera.grade().name, "FIRST LIGHT");
        camera.cycle_grade(-1);
        assert_eq!(camera.grade().name, "NEUTRAL");
        camera.cycle_grade(-1);
        assert_eq!(camera.grade().name, "MONO", "stepping back from the first wraps");
        camera.cycle_grade(1);
        assert_eq!(camera.grade().name, "NEUTRAL");
        camera.nudge_fov(-1000.0);
        assert_eq!(camera.fov_deg, FOV_MIN_DEG);
        camera.nudge_fov(1000.0);
        assert_eq!(camera.fov_deg, FOV_MAX_DEG);
        camera.nudge_roll(400.0);
        assert_eq!(camera.roll_deg, ROLL_MAX_DEG);
        camera.nudge_exposure(-10.0);
        assert_eq!(camera.exposure_ev, EXPOSURE_MIN_EV);
        camera.focus_m = FOCUS_DEFAULT_M;
        camera.nudge_focus(100.0);
        assert_eq!(camera.focus_m, FOCUS_DEFAULT_M * 4.0);
        for _ in 0..8 {
            camera.nudge_focus(100.0);
        }
        assert_eq!(camera.focus_m, FOCUS_MAX_M);
        camera.nudge_aperture(10.0);
        assert_eq!(camera.aperture_f, APERTURE_MAX_F);
        camera.look(0.0, 100_000.0);
        assert!(
            (camera.pitch - PITCH_LIMIT_RAD).abs() < 1.0e-6,
            "looking up stops short of the pole"
        );
        camera.look(f32::NAN, f32::NAN);
        assert!(
            camera.yaw.is_finite() && camera.pitch.is_finite(),
            "a bad mouse frame is ignored"
        );
        let mut still = PhotoCamera::default();
        still.step_photo(
            PhotoMove {
                fwd: f32::NAN,
                strafe: 99.0,
                up: -99.0,
                fast: false,
            },
            f32::NAN,
        );
        assert_eq!(
            still.offset, [0.0; 3],
            "bad rates and a bad step never move the eye"
        );
    }

    #[test]
    fn toggle_offline_opens_online_stays_on_a_short_leash_and_the_sim_is_not_paused() {
        let saved = saved_view();
        let mut state = PhotoState::default();
        let out = state.drive(
            solo_live(),
            saved,
            [0.0, 2.0, 0.0],
            PhotoKeys {
                toggle: true,
                ..PhotoKeys::default()
            },
        );
        assert!(out.opened && out.active, "offline toggle opens photo mode");
        assert!(out.hud_hidden);
        assert!(!out.pause_sim);
        assert!(out.neutral_drive);
        assert_eq!(out.denied, None);
        let mut state = PhotoState::default();
        let _ = state.drive(
            solo_live(),
            saved,
            [0.0; 3],
            PhotoKeys {
                toggle: true,
                ..PhotoKeys::default()
            },
        );
        let _ = state.drive(solo_live(), saved, [0.0; 3], fly_fast());
        let wide = offset_len(state.camera.offset);
        assert!(
            wide > ONLINE_RADIUS_M,
            "offline roam passes the online leash: {wide}"
        );
        assert!(
            wide <= RADIUS_M + 1.0e-3,
            "offline roam still ends on the wide shell: {wide}"
        );

        let mut state = PhotoState::default();
        let out = state.drive(
            online_live(),
            saved,
            [10.0, 1.0, -4.0],
            PhotoKeys {
                toggle: true,
                ..PhotoKeys::default()
            },
        );
        assert!(
            out.opened && out.active,
            "a live online match may open photo mode"
        );
        assert!(!out.pause_sim, "opening photo mode does not pause the online sim");
        assert!(out.neutral_drive);
        assert!((out.radius_m - ONLINE_RADIUS_M).abs() < 1.0e-4);
        let mut tick = 0u32;
        let mut paused = false;
        for _ in 0..5 {
            let frame = state.drive(online_live(), saved, [10.0, 1.0, -4.0], fly_fast());
            assert!(!frame.pause_sim);
            if frame.pause_sim {
                paused = true;
            } else {
                tick = tick.saturating_add(1);
            }
        }
        assert!(!paused && tick == 5, "five online frames still step the sim");
        let held = offset_len(state.camera.offset);
        assert!(
            held <= ONLINE_RADIUS_M + 1.0e-3,
            "online eye stays within {ONLINE_RADIUS_M} m: {held}"
        );
        let eye = state.camera.eye(state.anchor);
        let from_unit = offset_len([eye[0] - 10.0, eye[1] - 1.0, eye[2] + 4.0]);
        assert!(
            from_unit <= ONLINE_RADIUS_M + 1.0e-3,
            "the leash is around the unit: {from_unit}"
        );
        let followed = state.drive(online_live(), saved, [80.0, 1.0, 20.0], PhotoKeys::default());
        let from_new = offset_len([
            followed.eye[0] - 80.0,
            followed.eye[1] - 1.0,
            followed.eye[2] - 20.0,
        ]);
        assert!(
            from_new <= ONLINE_RADIUS_M + 1.0e-3,
            "the leash follows the unit: {from_new}"
        );

        let training = MatchAccess {
            networked: true,
            training: true,
            ..MatchAccess::default()
        };
        let mut state = PhotoState::default();
        let _ = state.drive(
            training,
            saved,
            [0.0; 3],
            PhotoKeys {
                toggle: true,
                ..PhotoKeys::default()
            },
        );
        let _ = state.drive(training, saved, [0.0; 3], fly_fast());
        let trained = offset_len(state.camera.offset);
        assert!(
            trained > ONLINE_RADIUS_M,
            "training keeps the wide roam: {trained}"
        );

        for access in [
            MatchAccess {
                networked: true,
                dead: true,
                ..MatchAccess::default()
            },
            MatchAccess {
                networked: true,
                spectating: true,
                ..MatchAccess::default()
            },
        ] {
            let mut state = PhotoState::default();
            let _ = state.drive(
                access,
                saved,
                [0.0; 3],
                PhotoKeys {
                    toggle: true,
                    ..PhotoKeys::default()
                },
            );
            let _ = state.drive(access, saved, [0.0; 3], fly_fast());
            let len = offset_len(state.camera.offset);
            assert!(
                len > ONLINE_RADIUS_M,
                "dead or spectating keeps the wide roam: {len}"
            );
            assert!(
                !state
                    .drive(access, saved, [0.0; 3], PhotoKeys::default())
                    .pause_sim
            );
        }
    }

    #[test]
    fn the_hud_hides_only_while_photo_mode_is_open() {
        let saved = saved_view();
        let mut state = PhotoState::default();
        let closed = state.drive(solo_live(), saved, [0.0; 3], PhotoKeys::default());
        assert!(!closed.hud_hidden && !closed.active);
        let open = state.drive(
            solo_live(),
            saved,
            [0.0; 3],
            PhotoKeys {
                toggle: true,
                ..PhotoKeys::default()
            },
        );
        assert!(open.hud_hidden && state.hud_hidden());
        let still = state.drive(online_live(), saved, [0.0; 3], PhotoKeys::default());
        assert!(
            still.hud_hidden,
            "the HUD stays hidden for the whole session, online included"
        );
        let back = state.drive(
            online_live(),
            saved,
            [0.0; 3],
            PhotoKeys {
                escape: true,
                ..PhotoKeys::default()
            },
        );
        assert!(!back.hud_hidden && !state.hud_hidden());
    }

    #[test]
    fn exit_on_the_toggle_and_on_escape() {
        let saved = saved_view();
        let mut state = PhotoState::default();
        let _ = state.drive(
            solo_live(),
            saved,
            [3.0, 1.0, 2.0],
            PhotoKeys {
                toggle: true,
                ..PhotoKeys::default()
            },
        );
        state.camera.nudge_fov(-10.0);
        let by_toggle = state.drive(
            solo_live(),
            saved,
            [3.0, 1.0, 2.0],
            PhotoKeys {
                toggle: true,
                ..PhotoKeys::default()
            },
        );
        assert!(by_toggle.exited && !by_toggle.active && !by_toggle.hud_hidden);
        assert!(!by_toggle.capture, "closing does not also take a still");
        assert!(state.exit().is_none(), "the toggle already restored the view");

        let _ = state.drive(
            online_live(),
            saved,
            [0.0; 3],
            PhotoKeys {
                toggle: true,
                ..PhotoKeys::default()
            },
        );
        let by_esc = state.drive(
            online_live(),
            saved,
            [0.0; 3],
            PhotoKeys {
                escape: true,
                ..PhotoKeys::default()
            },
        );
        assert!(by_esc.exited && !by_esc.active);
        assert!(
            !by_esc.pause_sim,
            "leaving a live online photo session does not pause the sim"
        );
        let ignored = state.drive(
            online_live(),
            saved,
            [0.0; 3],
            PhotoKeys {
                escape: true,
                ..PhotoKeys::default()
            },
        );
        assert!(
            !ignored.opened && !ignored.exited && !ignored.active,
            "escape does not open photo mode"
        );
        let again = state.drive(
            solo_live(),
            saved,
            [0.0; 3],
            PhotoKeys {
                toggle: true,
                ..PhotoKeys::default()
            },
        );
        assert!(again.opened);
    }

    #[test]
    fn screenshot_request_on_an_open_session_only() {
        let mut state = PhotoState::default();
        let saved = saved_view();
        let open = state.drive(
            solo_live(),
            saved,
            [0.0; 3],
            PhotoKeys {
                toggle: true,
                capture: true,
                ..PhotoKeys::default()
            },
        );
        assert!(open.capture, "the capture key takes a still on an open session");
        assert!(open.hud_hidden);
        let shut = state.drive(
            solo_live(),
            saved,
            [0.0; 3],
            PhotoKeys {
                toggle: true,
                capture: true,
                ..PhotoKeys::default()
            },
        );
        assert!(!shut.capture, "the frame that closes does not also capture");
    }

    #[test]
    fn movement_keys_fold_into_the_photo_axes() {
        let mv = move_from_held(true, false, false, true, true, false, true);
        assert_eq!(
            mv,
            PhotoMove {
                fwd: 1.0,
                strafe: -1.0,
                up: 1.0,
                fast: true
            }
        );
        let still = move_from_held(true, true, false, false, false, false, false);
        assert_eq!(still, PhotoMove::default(), "both directions held is neutral");
    }

    #[test]
    fn constant_ranges_match_the_source_contract() {
        assert!((5.0..=100.0).contains(&RADIUS_M));
        assert!((30.0..=360.0).contains(&ROLL_MAX_DEG));
        assert!((1.0..=20.0).contains(&MOVE_SPEED_M_S));
        const {
            assert!(
                10.0 <= FOV_MIN_DEG
                    && FOV_MIN_DEG < FOV_DEFAULT_DEG
                    && FOV_DEFAULT_DEG < FOV_MAX_DEG
                    && FOV_MAX_DEG <= 180.0
            );
            assert!(EXPOSURE_MIN_EV == -EXPOSURE_MAX_EV);
            assert!(EXPOSURE_MAX_EV <= 5.0);
            assert!(0.0 < FOCUS_MIN_M && FOCUS_MIN_M < FOCUS_DEFAULT_M && FOCUS_DEFAULT_M < FOCUS_MAX_M);
            assert!(
                1.0 <= APERTURE_MIN_F
                    && APERTURE_MIN_F < APERTURE_DEFAULT_F
                    && APERTURE_DEFAULT_F < APERTURE_MAX_F
                    && APERTURE_MAX_F <= 32.0
            );
            assert!(CAPTURE_MAX_DIM <= 16_384);
            let _ = FALLBACK_SIZE;
        }
    }

    #[test]
    fn look_rotation_points_regia_forward_along_the_view() {
        let dir = view_dir(0.4, -0.2);
        let rotation = look_rotation(dir, 0.3);
        let forward = rotation * Vec3::NEG_Z;
        let want = Vec3::from_array(dir).normalize();
        assert!((forward - want).length() < 1.0e-5, "{forward:?} vs {want:?}");
    }
}
