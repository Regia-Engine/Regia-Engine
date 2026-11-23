// SPDX-License-Identifier: MPL-2.0
//! Read-only views of a [`GaitState`] for debug draw and tools (the locomotion gizmos, ADR-8729):
//! each foot's swing, its neutral foothold, its landing point and the stepper's swing arc
//! formula. Nothing here changes the state; presentation only, never read by a checksum.

use super::{FootMode, Frame, GaitParams, GaitState, arc, hip};

/// Swing progress below which [`GaitState::swing_landing`] does not invert the arc (the foot
/// has barely left the ground, so the landing point is not yet determined by its position).
pub const MIN_LANDING_PROGRESS: f32 = 1.0e-3;

/// A foot in the air: where it lifted off and how far along its swing it is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FootSwing {
    /// World point the foot lifted off from, meters.
    pub lift_off: [f32; 3],
    /// Progress along the swing in `[0, 1]`.
    pub progress: f32,
    /// `true` for a forced out-of-phase step ([`FootMode::Forced`]), `false` for a pattern swing.
    pub forced: bool,
}

impl GaitState {
    /// The swing of foot `leg` while it is in the air on an arc: a pattern swing (also toward a
    /// hang point) or a forced step. `None` for a planted or waiting foot, a pattern swing held
    /// at its lift-off point (a refused landing), or a leg past [`Self::leg_count`].
    #[must_use]
    pub fn swing(&self, leg: usize) -> Option<FootSwing> {
        if leg >= self.leg_count() {
            return None;
        }
        match self.foot_modes[leg] {
            FootMode::Forced => Some(FootSwing {
                lift_off: self.lift_off[leg],
                progress: self.forced_phase[leg],
                forced: true,
            }),
            FootMode::Waiting => None,
            FootMode::Tripod | FootMode::Hanging => (self.swinging[leg] && !self.feet[leg].planted)
                .then_some(FootSwing {
                    lift_off: self.lift_off[leg],
                    progress: self.swing_progress[leg],
                    forced: false,
                }),
        }
    }

    /// Foot `leg`'s neutral foothold in world meters: its hip offset turned by the body yaw in
    /// the state's frame ([`super::GaitMode`]), at the expected floor (body height minus
    /// `clearance`). `None` past [`Self::leg_count`].
    #[must_use]
    pub fn neutral_foothold(&self, params: &GaitParams, leg: usize) -> Option<[f32; 3]> {
        if leg >= self.leg_count() {
            return None;
        }
        let frame = Frame::of(self.mode, self.yaw);
        let [x, z] = hip(self.position, frame, params.hip_offsets[leg]);
        Some([x, self.position[1] - params.clearance, z])
    }

    /// Where foot `leg`'s current swing lands, recovered from its position on the stepper's arc
    /// ([`swing_arc_point`]) and its lift-off point. `None` when the foot is not swinging (see
    /// [`Self::swing`]) or is within [`MIN_LANDING_PROGRESS`] of its lift-off.
    #[must_use]
    pub fn swing_landing(&self, params: &GaitParams, leg: usize) -> Option<[f32; 3]> {
        let swing = self.swing(leg)?;
        let u = swing.progress;
        if !(MIN_LANDING_PROGRESS..=1.0).contains(&u) {
            return None;
        }
        let p = self.feet[leg].position;
        let s = swing.lift_off;
        let bump = 4.0 * u * (1.0 - u) * params.swing_lift;
        let landing = [
            s[0] + (p[0] - s[0]) / u,
            s[1] + (p[1] - bump - s[1]) / u,
            s[2] + (p[2] - s[2]) / u,
        ];
        landing.iter().all(|v| v.is_finite()).then_some(landing)
    }
}

/// The stepper's swing arc: the point at progress `u` of a swing from `start` to `target`,
/// `swing_lift` above the straight line halfway (a parabola). This is the formula the stepper
/// moves its feet by, so a sampled arc lies exactly on the foot's path.
#[must_use]
pub fn swing_arc_point(params: &GaitParams, start: [f32; 3], target: [f32; 3], u: f32) -> [f32; 3] {
    arc(params, start, target, u)
}
