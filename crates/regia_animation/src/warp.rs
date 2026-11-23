// SPDX-License-Identifier: MPL-2.0
//! Motion warping: bend a montage's root motion at runtime so the rig lands on a target.
//!
//! Specification: `docs/specs/17-animation.md` § 4b (ADR-1465). Design from the public motion
//! warping and motion path editing literature (Witkin and Popović 1995, Gleicher 2001; see
//! `docs/provenance.md`): the authored motion is kept and a smooth correction is added so its
//! end meets a new constraint.
//!
//! Every frame inside a warp window the player knows the root motion still to come in the
//! window (this frame's plus the rest of the window, as authored) and the offset from the rig's
//! current position to the target. [`warp_frame`] maps this frame's motion with the similarity
//! transform (a yaw rotation and a uniform horizontal scale) that takes the remaining authored
//! path onto the remaining offset, so the path keeps its shape; when the authored path has no
//! horizontal length to scale, the missing offset is spread linearly over the remaining window
//! time instead. Height and facing are spread linearly. The correction is recomputed from the
//! current position every frame, so a moving target is tracked and the error at the window end
//! is what the last frame could not cover (zero when frames line up with the window end).
//!
//! All quantities are in the rig's object space (Y up, −Z forward).

use regia_core::math::{Quat, Vec3, det};
use serde::{Deserialize, Serialize};

/// How a warp window maps the remaining authored horizontal path onto the remaining offset
/// (`docs/specs/34-montage-extensions.md` § 4).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum WarpMode {
    /// Rotate and uniformly scale the path (the original behavior).
    #[default]
    Similarity,
    /// Uniformly scale the path without rotating it: it keeps its authored heading.
    PathScale,
}

/// Which facing a rotation warp turns the rig to.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum RotationKind {
    /// Match the target's facing when it has one, else face the target position.
    #[default]
    Auto,
    /// Match the target's facing; no rotation when the target has none.
    MatchTarget,
    /// Face the target position, ignoring the target's facing.
    FaceTarget,
}

/// What a warp window lands on the target at its end.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum WarpAnchor {
    /// The rig origin.
    #[default]
    Root,
    /// The named bone, at its position in the authored pose at the window end.
    Bone(String),
    /// A fixed offset from the origin in rig space.
    Offset([f32; 3]),
}

/// Below this horizontal length (metres) the authored path is treated as having no horizontal
/// motion and the offset is spread linearly instead of scaled.
pub const MIN_SCALABLE_LENGTH: f32 = 1e-3;

/// Where a warp window should take the rig, in world space.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WarpTarget {
    /// Target position of the rig's origin.
    pub position: Vec3,
    /// Facing at the target (only the yaw is used); `None` = face toward `position`.
    pub facing: Option<Quat>,
}

impl WarpTarget {
    /// A target position, facing toward it.
    pub fn at(position: Vec3) -> Self {
        WarpTarget {
            position,
            facing: None,
        }
    }
}

/// What one warp step needs, all in rig object space at the start of the frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WarpInput {
    /// This frame's root motion `(translation, rotation)` before warping.
    pub frame: (Vec3, Quat),
    /// The authored root motion of the rest of the window after this frame (in the frame this
    /// frame's rotation reaches).
    pub rest: (Vec3, Quat),
    /// Offset from the rig's current origin to the target.
    pub to_target: Vec3,
    /// Yaw the rig still has to turn to face the target (radians, counter-clockwise about +Y);
    /// `None` = no rotation warp.
    pub yaw_to_target: Option<f32>,
    /// Montage seconds this frame covers inside the window.
    pub frame_time: f32,
    /// Montage seconds left in the window after this frame.
    pub rest_time: f32,
    /// Warp the horizontal translation.
    pub horizontal: bool,
    /// Warp the height.
    pub vertical: bool,
    /// Path mapping of the horizontal warp.
    pub mode: WarpMode,
    /// Montage seconds of this frame that fall inside the rotation part of the window
    /// (equal to `frame_time` when the rotation spans the whole window).
    pub rotation_frame_time: f32,
    /// Montage seconds of the rotation part left after this frame (equal to `rest_time` when
    /// the rotation spans the whole window).
    pub rotation_rest_time: f32,
}

/// The yaw (radians about +Y) of a rotation. Uses the deterministic `regia_core::math::det`
/// kernel so the bits match on every platform (audit-v040 M3).
pub fn yaw_of(rotation: Quat) -> f32 {
    det::quat_to_euler_yxz(rotation).0
}

/// The yaw that turns the rig's −Z forward axis toward `direction` (horizontal part).
pub fn yaw_toward(direction: Vec3) -> Option<f32> {
    let flat = Vec3::new(direction.x, 0.0, direction.z);
    (flat.length_squared() > 1e-12).then(|| det::atan2(-flat.x, -flat.z))
}

/// `angle` wrapped to `(-π, π]`; a non-finite angle wraps to `0`.
pub fn wrap_angle(angle: f32) -> f32 {
    if !angle.is_finite() {
        return 0.0;
    }
    let tau = std::f32::consts::TAU;
    let mut a = angle.rem_euclid(tau);
    if a > std::f32::consts::PI {
        a -= tau;
    }
    a
}

/// No root motion.
pub(crate) const NO_ROOT_MOTION: (Vec3, Quat) = (Vec3::ZERO, Quat::IDENTITY);

/// `first` then `next`: the translation of `next` is expressed in the frame `first`'s rotation
/// reached (how root-motion deltas accumulate on a rig).
pub(crate) fn compose_root(first: (Vec3, Quat), next: (Vec3, Quat)) -> (Vec3, Quat) {
    (first.0 + first.1 * next.0, (first.1 * next.1).normalize())
}

/// The delta that undoes `delta`: `compose_root(delta, invert_root(delta))` is no motion.
pub(crate) fn invert_root(delta: (Vec3, Quat)) -> (Vec3, Quat) {
    let inverse = delta.1.inverse();
    (-(inverse * delta.0), inverse)
}

/// The middle piece of a step's root motion split at warp-window edges (ADR-2604): `whole`
/// is `before`, then the part inside the window, then `after`, so the inside part is
/// `before⁻¹ · whole · after⁻¹`. With `before` and `after` both no motion it is `whole`.
pub(crate) fn inside_part(whole: (Vec3, Quat), before: (Vec3, Quat), after: (Vec3, Quat)) -> (Vec3, Quat) {
    compose_root(compose_root(invert_root(before), whole), invert_root(after))
}

/// Warp one frame of root motion. Returns the corrected `(translation, rotation)`.
///
/// `frame` must be only the part of the step that lies inside the warp window: a caller whose
/// step straddles a window edge splits the step's root motion at the edge, warps the inside
/// part and passes the outside parts through (`inside_part` and
/// `docs/specs/34-montage-extensions.md` § 4a, ADR-2604).
///
/// A non-finite input or a frame that covers no time returns the frame unchanged.
pub fn warp_frame(input: &WarpInput) -> (Vec3, Quat) {
    let (t, r) = input.frame;
    let valid = t.is_finite()
        && r.is_finite()
        && input.rest.0.is_finite()
        // The rotation branch folds `rest.1` into the output (audit-gameplay Low).
        && input.rest.1.is_finite()
        && input.to_target.is_finite()
        && input.frame_time.is_finite()
        && input.rest_time.is_finite()
        && input.yaw_to_target.is_none_or(f32::is_finite)
        && input.rotation_frame_time.is_finite()
        && input.rotation_rest_time.is_finite();
    if !valid || input.frame_time <= 0.0 {
        return input.frame;
    }
    let total_time = input.frame_time + input.rest_time.max(0.0);
    let share = (input.frame_time / total_time).clamp(0.0, 1.0);
    // The authored remaining motion, this frame included.
    let remaining = t + r * input.rest.0;
    let mut out = t;
    if input.horizontal {
        let a = Vec3::new(remaining.x, 0.0, remaining.z);
        let b = Vec3::new(input.to_target.x, 0.0, input.to_target.z);
        let flat = Vec3::new(t.x, 0.0, t.z);
        let warped = if a.length() > MIN_SCALABLE_LENGTH {
            // Similarity taking `a` onto `b`: rotate by the angle between them, scale by the ratio.
            let angle = match input.mode {
                WarpMode::Similarity => wrap_angle(det::atan2(-b.x, -b.z) - det::atan2(-a.x, -a.z)),
                WarpMode::PathScale => 0.0,
            };
            let scale = b.length() / a.length();
            if b.length() > MIN_SCALABLE_LENGTH {
                det::quat_from_rotation_y(angle) * flat * scale
            } else {
                // The target is already reached: stop moving horizontally.
                Vec3::ZERO
            }
        } else {
            flat + (b - a) * share
        };
        out.x = warped.x;
        out.z = warped.z;
    }
    if input.vertical {
        out.y = t.y + (input.to_target.y - remaining.y) * share;
    }
    let rotation_time = input.rotation_frame_time + input.rotation_rest_time.max(0.0);
    let rotation_share = if rotation_time > 0.0 {
        (input.rotation_frame_time / rotation_time).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let rotation = match input.yaw_to_target {
        Some(needed) if needed.is_finite() && input.rotation_frame_time > 0.0 => {
            let authored = yaw_of(r * input.rest.1);
            let yaw = yaw_of(r) + wrap_angle(needed - authored) * rotation_share;
            det::quat_from_rotation_y(yaw)
        }
        _ => r,
    };
    (out, rotation)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Play an authored straight dash of `steps` equal frames toward −Z with warping, the rig
    /// starting at the origin; returns where it ends.
    fn run(target: Vec3, dash: Vec3, steps: usize) -> Vec3 {
        let per_frame = dash / steps as f32;
        let dt = 0.1;
        let mut position = Vec3::ZERO;
        for i in 0..steps {
            let rest_frames = (steps - i - 1) as f32;
            let (t, _) = warp_frame(&WarpInput {
                frame: (per_frame, Quat::IDENTITY),
                rest: (per_frame * rest_frames, Quat::IDENTITY),
                to_target: target - position,
                yaw_to_target: None,
                frame_time: dt,
                rest_time: dt * rest_frames,
                horizontal: true,
                vertical: false,
                mode: WarpMode::Similarity,
                rotation_frame_time: dt,
                rotation_rest_time: dt * rest_frames,
            });
            position += t;
        }
        position
    }

    #[test]
    fn a_non_finite_rest_rotation_passes_the_frame_through() {
        let frame = (Vec3::new(0.0, 0.0, -0.1), Quat::IDENTITY);
        let out = warp_frame(&WarpInput {
            frame,
            rest: (
                Vec3::new(0.0, 0.0, -1.0),
                Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0),
            ),
            to_target: Vec3::new(1.0, 0.0, -2.0),
            yaw_to_target: Some(0.5),
            frame_time: 0.1,
            rest_time: 1.0,
            horizontal: true,
            vertical: false,
            mode: WarpMode::Similarity,
            rotation_frame_time: 0.1,
            rotation_rest_time: 1.0,
        });
        assert_eq!(
            out, frame,
            "zai L1 (ADR-1809): a NaN rest rotation is not folded into the output"
        );
    }

    #[test]
    fn a_straight_dash_is_scaled_and_turned_onto_the_target() {
        let target = Vec3::new(2.0, 0.0, -3.0);
        let end = run(target, Vec3::new(0.0, 0.0, -2.0), 10);
        assert!((end - target).length() < 1e-4, "{end}");
    }

    #[test]
    fn no_authored_motion_spreads_the_offset_linearly() {
        let target = Vec3::new(0.0, 0.0, -1.0);
        let end = run(target, Vec3::ZERO, 4);
        assert!((end - target).length() < 1e-5, "{end}");
    }

    #[test]
    fn rotation_warp_reaches_the_needed_yaw() {
        let steps = 8;
        let mut yaw = 0.0f32;
        let goal = 1.2f32;
        for i in 0..steps {
            let rest = (steps - i - 1) as f32;
            let (_, r) = warp_frame(&WarpInput {
                frame: (Vec3::ZERO, Quat::IDENTITY),
                rest: (Vec3::ZERO, Quat::IDENTITY),
                to_target: Vec3::ZERO,
                yaw_to_target: Some(goal - yaw),
                frame_time: 0.05,
                rest_time: 0.05 * rest,
                horizontal: false,
                vertical: false,
                mode: WarpMode::Similarity,
                rotation_frame_time: 0.05,
                rotation_rest_time: 0.05 * rest,
            });
            yaw += yaw_of(r);
        }
        assert!((yaw - goal).abs() < 1e-4, "{yaw}");
    }

    #[test]
    fn non_finite_input_passes_the_frame_through() {
        let frame = (Vec3::X, Quat::IDENTITY);
        let out = warp_frame(&WarpInput {
            frame,
            rest: (Vec3::ZERO, Quat::IDENTITY),
            to_target: Vec3::splat(f32::NAN),
            yaw_to_target: None,
            frame_time: 0.1,
            rest_time: 0.1,
            horizontal: true,
            vertical: true,
            mode: WarpMode::Similarity,
            rotation_frame_time: 0.1,
            rotation_rest_time: 0.1,
        });
        assert_eq!(out, frame);
        // A NaN rest rotation with a yaw goal used to come back as a NaN rotation.
        let out = warp_frame(&WarpInput {
            frame,
            rest: (Vec3::ZERO, Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0)),
            to_target: Vec3::ZERO,
            yaw_to_target: Some(1.0),
            frame_time: 0.1,
            rest_time: 0.1,
            horizontal: true,
            vertical: true,
            mode: WarpMode::Similarity,
            rotation_frame_time: 0.1,
            rotation_rest_time: 0.1,
        });
        assert_eq!(out, frame);
    }

    #[test]
    fn yaw_toward_matches_the_forward_axis() {
        assert!(yaw_toward(Vec3::new(0.0, 0.0, -1.0)).is_some_and(|y| y.abs() < 1e-6));
        let left = yaw_toward(Vec3::new(-1.0, 0.0, 0.0)).unwrap_or_default();
        // Turning +90° about +Y takes −Z to −X.
        assert!((left - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        assert!(yaw_toward(Vec3::Y).is_none());
    }

    #[test]
    fn path_scale_keeps_the_authored_heading_of_every_frame() {
        let steps = 10;
        let per_frame = Vec3::new(0.0, 0.0, -0.3);
        let target = Vec3::new(1.0, 0.0, -5.0);
        let mut position = Vec3::ZERO;
        for i in 0..steps {
            let rest_frames = (steps - i - 1) as f32;
            let (t, _) = warp_frame(&WarpInput {
                frame: (per_frame, Quat::IDENTITY),
                rest: (per_frame * rest_frames, Quat::IDENTITY),
                to_target: target - position,
                yaw_to_target: None,
                frame_time: 0.1,
                rest_time: 0.1 * rest_frames,
                horizontal: true,
                vertical: false,
                mode: WarpMode::PathScale,
                rotation_frame_time: 0.1,
                rotation_rest_time: 0.1 * rest_frames,
            });
            assert!(t.x.abs() < 1e-6, "frame {i} turned: {t}");
            assert!(t.z < 0.0, "frame {i} reversed: {t}");
            position += t;
        }
    }

    /// Run a pure rotation warp of `goal` radians over `steps` frames whose rotation part
    /// covers the first `fraction` of the window; returns the yaw after each frame.
    fn yaw_trace(goal: f32, steps: usize, fraction: f32) -> Vec<f32> {
        let dt = 0.1;
        let window = dt * steps as f32;
        let rotation_end = window * fraction;
        let mut yaw = 0.0f32;
        let mut trace = Vec::new();
        for i in 0..steps {
            let from = dt * i as f32;
            let to = from + dt;
            let rest = (window - to).max(0.0);
            let (_, r) = warp_frame(&WarpInput {
                frame: (Vec3::ZERO, Quat::IDENTITY),
                rest: (Vec3::ZERO, Quat::IDENTITY),
                to_target: Vec3::ZERO,
                yaw_to_target: Some(goal - yaw),
                frame_time: dt,
                rest_time: rest,
                horizontal: false,
                vertical: false,
                mode: WarpMode::Similarity,
                rotation_frame_time: (rotation_end.min(to) - from).max(0.0),
                rotation_rest_time: (rotation_end - to).max(0.0),
            });
            yaw += yaw_of(r);
            trace.push(yaw);
        }
        trace
    }

    #[test]
    fn rotation_fraction_reaches_the_yaw_at_the_fraction_and_holds() {
        let goal = 1.0;
        let trace = yaw_trace(goal, 10, 0.5);
        assert!((trace[4] - goal).abs() < 1e-4, "{trace:?}");
        for yaw in &trace[5..] {
            assert!((yaw - goal).abs() < 1e-4, "{trace:?}");
        }
        assert!(trace[1] < goal * 0.9, "{trace:?}");
    }

    /// A non-finite facing or rotation time returns the frame unchanged, translation included.
    #[test]
    fn non_finite_yaw_or_rotation_time_leaves_the_frame_alone() {
        let base = WarpInput {
            frame: (Vec3::new(0.0, 0.0, -1.0), Quat::IDENTITY),
            rest: (Vec3::ZERO, Quat::IDENTITY),
            to_target: Vec3::new(2.0, 0.0, 0.0),
            frame_time: 0.1,
            rest_time: 0.0,
            horizontal: true,
            vertical: false,
            mode: WarpMode::Similarity,
            yaw_to_target: None,
            rotation_frame_time: 0.1,
            rotation_rest_time: 0.0,
        };
        for bad in [
            WarpInput {
                yaw_to_target: Some(f32::NAN),
                ..base
            },
            WarpInput {
                rotation_frame_time: f32::NAN,
                ..base
            },
            WarpInput {
                rotation_rest_time: f32::INFINITY,
                ..base
            },
        ] {
            assert_eq!(warp_frame(&bad), bad.frame);
        }
    }
}
