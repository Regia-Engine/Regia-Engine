// SPDX-License-Identifier: MPL-2.0
//! Sampling a [`ShotSequence`] at a time: a pure function from `(sequence, seconds)` to a camera
//! pose, lens, depth of field and fade.
//!
//! No clocks, randomness or hidden state: the same sequence and the same time give the same
//! [`ShotSample`] on every call, machine and frame rate. Fixed-step playback is
//! `sample(frame as f64 / fps)`; [`time_of_frame`] is that division.

use regia_core::glam::DVec3;
use regia_core::math::{Mat3, Quat, Vec3};

use super::asset::{
    CameraPath, DEFAULT_APERTURE, DEFAULT_FOCUS_DISTANCE, DEFAULT_FOV_DEGREES, Ease, LookAt, PathInterp,
    PathKey, RotationKey, ScalarKey, Shot, ShotSequence, Track,
};
use super::spline::{catmull_rom_segment, catmull_rom_segment_f64};

/// Half-width, seconds, of the finite difference that gives the direction of travel.
const HEADING_STEP: f64 = 1.0 / 120.0;

/// How far ahead of the eye, metres, `LookAt::Heading` reports its target.
const HEADING_REACH: f32 = 10.0;

/// The time of frame number `frame` at `fps` frames per second.
#[must_use]
pub fn time_of_frame(frame: u64, fps: f32) -> f64 {
    frame as f64 / f64::from(fps)
}

/// Depth of field at one instant, ready for [`regia_render::PhysicalCamera`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DofSample {
    /// Distance to the plane in focus, metres.
    pub focus_distance: f32,
    /// Aperture, f-stops.
    pub aperture: f32,
    /// Hexagonal bokeh instead of a Gaussian blur.
    pub bokeh: bool,
}

/// Everything a sequence says about one instant.
#[derive(Clone, Debug, PartialEq)]
pub struct ShotSample {
    /// The (clamped) sequence time this sample is for, seconds.
    pub time: f64,
    /// Index of the active shot in [`ShotSequence::shots`].
    pub shot: usize,
    /// Seconds since the start of the active shot.
    pub local_time: f32,
    /// Camera position, metres.
    pub eye: Vec3,
    /// The point the camera looks at. For `LookAt::Heading`, a point 10 m ahead.
    pub target: Vec3,
    /// The camera's up vector after roll.
    pub up: Vec3,
    /// Camera orientation (looks down −Z, +Y up), roll applied.
    pub rotation: Quat,
    /// Roll about the view axis, degrees.
    pub roll_degrees: f32,
    /// Vertical field of view, degrees.
    pub fov_y_degrees: f32,
    /// Depth of field, `None` when the shot has none.
    pub dof: Option<DofSample>,
    /// Opacity of the fade overlay: 0 = picture only, 1 = fully `fade_color`.
    pub fade: f32,
    /// Linear RGB of the fade overlay.
    pub fade_color: [f32; 3],
    /// `true` once the time has reached the end of the sequence.
    pub finished: bool,
}

impl Track {
    /// The value at `time`, `None` for an empty track. Holds the first value before the first key
    /// and the last value after the last.
    #[must_use]
    pub fn sample(&self, time: f32) -> Option<f32> {
        let keys = &self.0;
        let (first, last) = (keys.first()?, keys.last()?);
        if time <= first.time {
            return Some(first.value);
        }
        if time >= last.time {
            return Some(last.value);
        }
        // A NaN key time fails both endpoint tests; hold the first value then.
        let i = keys.partition_point(|k| k.time <= time).saturating_sub(1);
        let Some(b) = keys.get(i + 1) else {
            return Some(first.value);
        };
        Some(segment_value(&keys[i], b, time))
    }

    /// [`Self::sample`] or `default` for an empty track.
    #[must_use]
    pub fn sample_or(&self, time: f32, default: f32) -> f32 {
        self.sample(time).unwrap_or(default)
    }
}

fn segment_value(a: &ScalarKey, b: &ScalarKey, time: f32) -> f32 {
    let span = b.time - a.time;
    let u = if span > 0.0 {
        ((time - a.time) / span).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let u = match a.ease {
        Ease::Linear => u,
        Ease::Smooth => u * u * (3.0 - 2.0 * u),
        Ease::Step => return a.value,
    };
    a.value + (b.value - a.value) * u
}

fn vec3(p: [f32; 3]) -> Vec3 {
    Vec3::from_array(p)
}

/// The interpolated position of a key list at `time` (see [`PathInterp`]).
fn sample_keys(keys: &[PathKey], interp: PathInterp, time: f32) -> Vec3 {
    let (Some(first), Some(last)) = (keys.first(), keys.last()) else {
        return Vec3::ZERO;
    };
    if time <= first.time {
        return vec3(first.position);
    }
    if time >= last.time {
        return vec3(last.position);
    }
    let i = keys.partition_point(|k| k.time <= time).saturating_sub(1);
    let Some(b) = keys.get(i + 1) else {
        return vec3(first.position);
    };
    let a = &keys[i];
    let span = b.time - a.time;
    let u = if span > 0.0 {
        ((time - a.time) / span).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let (pa, pb) = (vec3(a.position), vec3(b.position));
    match interp {
        PathInterp::Step => pa,
        PathInterp::Linear => pa.lerp(pb, u),
        PathInterp::CatmullRom => {
            let before = if i == 0 {
                pa * 2.0 - pb
            } else {
                vec3(keys[i - 1].position)
            };
            let after = if i + 2 < keys.len() {
                vec3(keys[i + 2].position)
            } else {
                pb * 2.0 - pa
            };
            catmull_rom_segment(before, pa, pb, after, u)
        }
    }
}

/// [`sample_keys`] in `f64`, for the heading: its finite difference spans 1/60 s of travel, a
/// few centimetres, which `f32` positions kilometres from the origin round away (ADR-1488).
fn sample_keys_f64(keys: &[PathKey], interp: PathInterp, time: f64) -> DVec3 {
    let dvec3 = |p: [f32; 3]| Vec3::from_array(p).as_dvec3();
    let (Some(first), Some(last)) = (keys.first(), keys.last()) else {
        return DVec3::ZERO;
    };
    if time <= f64::from(first.time) {
        return dvec3(first.position);
    }
    if time >= f64::from(last.time) {
        return dvec3(last.position);
    }
    let i = keys
        .partition_point(|k| f64::from(k.time) <= time)
        .saturating_sub(1);
    let Some(b) = keys.get(i + 1) else {
        return dvec3(first.position);
    };
    let a = &keys[i];
    let span = f64::from(b.time) - f64::from(a.time);
    let u = if span > 0.0 {
        ((time - f64::from(a.time)) / span).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let (pa, pb) = (dvec3(a.position), dvec3(b.position));
    match interp {
        PathInterp::Step => pa,
        PathInterp::Linear => pa.lerp(pb, u),
        PathInterp::CatmullRom => {
            let before = if i == 0 {
                pa * 2.0 - pb
            } else {
                dvec3(keys[i - 1].position)
            };
            let after = if i + 2 < keys.len() {
                dvec3(keys[i + 2].position)
            } else {
                pb * 2.0 - pa
            };
            catmull_rom_segment_f64(before, pa, pb, after, u)
        }
    }
}

impl CameraPath {
    /// The camera position at shot time `time`.
    #[must_use]
    pub fn position(&self, time: f32) -> Vec3 {
        sample_keys(&self.keys, self.interp, time)
    }

    /// The direction of travel at `time`: the central difference of the position over ±1/120 s,
    /// clamped to the path's time range. A path that is standing still there falls back to the
    /// direction from its first to its last key, then to −Z.
    ///
    /// The difference is taken in `f64` (ADR-1488): in `f32`, two samples a few centimetres apart
    /// on a path kilometres from the origin differ mostly by rounding.
    #[must_use]
    pub fn heading(&self, time: f32) -> Vec3 {
        let (Some(first), Some(last)) = (self.keys.first(), self.keys.last()) else {
            return Vec3::NEG_Z;
        };
        let time = f64::from(time);
        let a = sample_keys_f64(
            &self.keys,
            self.interp,
            (time - HEADING_STEP).max(f64::from(first.time)),
        );
        let b = sample_keys_f64(
            &self.keys,
            self.interp,
            (time + HEADING_STEP).min(f64::from(last.time)),
        );
        let step = b - a;
        if step.length_squared() > 1.0e-12 {
            return step.normalize().as_vec3();
        }
        let overall = vec3(last.position) - vec3(first.position);
        if overall.length_squared() > 1.0e-12 {
            overall.normalize()
        } else {
            Vec3::NEG_Z
        }
    }

    /// The point the camera looks at, given the eye already computed for `time`.
    #[must_use]
    pub fn target(&self, time: f32, eye: Vec3) -> Vec3 {
        match &self.look_at {
            LookAt::Point(p) => vec3(*p),
            LookAt::Track(keys) => sample_keys(keys, self.interp, time),
            LookAt::Heading => eye + self.heading(time) * HEADING_REACH,
        }
    }
}

/// The orientation of a camera at `eye` looking at `target` with `up` as the horizon direction
/// and `roll_degrees` of roll. Looks down −Z. A target at the eye, or straight along `up`, falls
/// back to a fixed frame instead of producing NaN.
///
/// The direction is `target − eye` in `f32`: keep `target` well away from an eye far from the
/// origin, or pass the direction itself to [`look_rotation_along`] (ADR-1488).
#[must_use]
pub fn look_rotation(eye: Vec3, target: Vec3, up: Vec3, roll_degrees: f32) -> (Quat, Vec3) {
    look_rotation_along(target - eye, up, roll_degrees)
}

/// [`look_rotation`] from the view direction `forward` (any length) instead of a look-at point.
/// A zero `forward` looks along −Z.
#[must_use]
pub fn look_rotation_along(forward: Vec3, up: Vec3, roll_degrees: f32) -> (Quat, Vec3) {
    let forward = if forward.length_squared() > 1.0e-12 {
        forward.normalize()
    } else {
        Vec3::NEG_Z
    };
    let mut horizon = if up.length_squared() > 1.0e-12 {
        up.normalize()
    } else {
        Vec3::Y
    };
    if forward.dot(horizon).abs() > 0.9999 {
        horizon = if horizon.dot(Vec3::Z).abs() > 0.9999 {
            Vec3::X
        } else {
            Vec3::Z
        };
    }
    let right = forward.cross(horizon).normalize();
    let level_up = right.cross(forward);
    let roll = Quat::from_axis_angle(forward, roll_degrees.to_radians());
    let (right, up) = (roll * right, roll * level_up);
    let rotation = Quat::from_mat3(&Mat3::from_cols(right, up, -forward)).normalize();
    (rotation, up)
}

/// The unit quaternion of a rotation key (validated keys are never zero; a degenerate one reads
/// as the identity so a hand-built sequence cannot produce NaN).
fn key_rotation(key: &RotationKey) -> Quat {
    let [x, y, z, w] = key.rotation;
    let q = Quat::from_xyzw(x, y, z, w);
    if q.length_squared() > 1.0e-12 && q.is_finite() {
        q.normalize()
    } else {
        Quat::IDENTITY
    }
}

/// The orientation of a rotation track at shot time `time`, `None` for an empty track. Holds the
/// first orientation before the first key and the last after the last; between two keys it is a
/// spherical interpolation along the shortest arc (`Ease::Smooth` smoothsteps the parameter,
/// `Ease::Step` holds the earlier key).
#[must_use]
pub fn sample_rotation(keys: &[RotationKey], time: f32) -> Option<Quat> {
    let (first, last) = (keys.first()?, keys.last()?);
    if time <= first.time {
        return Some(key_rotation(first));
    }
    if time >= last.time {
        return Some(key_rotation(last));
    }
    let i = keys.partition_point(|k| k.time <= time).saturating_sub(1);
    let Some(b) = keys.get(i + 1) else {
        return Some(key_rotation(first));
    };
    let a = &keys[i];
    let span = b.time - a.time;
    let u = if span > 0.0 {
        ((time - a.time) / span).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let u = match a.ease {
        Ease::Linear => u,
        Ease::Smooth => u * u * (3.0 - 2.0 * u),
        Ease::Step => return Some(key_rotation(a)),
    };
    let (qa, mut qb) = (key_rotation(a), key_rotation(b));
    if qa.dot(qb) < 0.0 {
        // q and −q are the same orientation: take the short way round.
        qb = -qb;
    }
    Some(qa.slerp(qb, u).normalize())
}

/// Applies `roll_degrees` about the view axis to a keyed orientation; also returns the up vector.
fn roll_rotation(base: Quat, roll_degrees: f32) -> (Quat, Vec3) {
    let forward = base * Vec3::NEG_Z;
    let rotation = (Quat::from_axis_angle(forward, roll_degrees.to_radians()) * base).normalize();
    (rotation, rotation * Vec3::Y)
}

/// Opacity of the fade overlay `local` seconds into a shot.
fn fade_amount(shot: &Shot, local: f32) -> f32 {
    let mut fade = 0.0f32;
    if shot.fade_in > 0.0 && local < shot.fade_in {
        fade = fade.max(1.0 - local / shot.fade_in);
    }
    let remaining = shot.duration - local;
    if shot.fade_out > 0.0 && remaining < shot.fade_out {
        fade = fade.max(1.0 - remaining / shot.fade_out);
    }
    fade.clamp(0.0, 1.0)
}

impl ShotSequence {
    /// Total length in seconds.
    #[must_use]
    pub fn duration(&self) -> f64 {
        self.shots.iter().map(|s| f64::from(s.duration)).sum()
    }

    /// Start time, seconds, of shot `index`: the sum of the durations before it (an index past the
    /// end gives the total duration).
    #[must_use]
    pub fn shot_start(&self, index: usize) -> f64 {
        self.shots.iter().take(index).map(|s| f64::from(s.duration)).sum()
    }

    /// Index of the shot with this name.
    #[must_use]
    pub fn shot_index(&self, name: &str) -> Option<usize> {
        self.shots.iter().position(|s| s.name == name)
    }

    /// Finds the shot playing at `time` and the seconds into it. Time is clamped to
    /// `0..=duration`; a non-finite time reads as 0. A shot boundary belongs to the shot that
    /// starts there (cuts are half-open on the right), and the end of the sequence holds the last
    /// frame of the last shot. `None` for a sequence without shots.
    #[must_use]
    pub fn locate(&self, time: f64) -> Option<(usize, f32)> {
        let last = self.shots.len().checked_sub(1)?;
        let total = self.duration();
        // Unvalidated durations (NaN, negative) would make `clamp` abort: no locate then.
        if !total.is_finite() || self.shots.iter().any(|s| s.duration.is_nan() || s.duration < 0.0) {
            return None;
        }
        let time = if time.is_finite() {
            time.clamp(0.0, total)
        } else {
            0.0
        };
        let mut start = 0.0f64;
        for (index, shot) in self.shots.iter().enumerate() {
            let end = start + f64::from(shot.duration);
            if time < end || index == last {
                let local = ((time - start) as f32).clamp(0.0, shot.duration);
                return Some((index, local));
            }
            start = end;
        }
        None
    }

    /// The camera, lens, depth of field and fade at sequence time `time` seconds. `None` for a
    /// sequence without shots. Deterministic: a pure function of `(self, time)`.
    #[must_use]
    pub fn sample(&self, time: f64) -> Option<ShotSample> {
        let (index, local) = self.locate(time)?;
        let shot = &self.shots[index];
        let path = &shot.camera.path;
        let total = self.duration();
        // Unvalidated durations (NaN, negative) would make `clamp` abort: no locate then.
        if !total.is_finite() || self.shots.iter().any(|s| s.duration.is_nan() || s.duration < 0.0) {
            return None;
        }
        let time = if time.is_finite() {
            time.clamp(0.0, total)
        } else {
            0.0
        };

        let eye = path.position(local);
        // A heading shot turns with its direction of travel itself, not with the point 10 m
        // ahead: that point's f32 rounding far from the origin quantized the view (ADR-1488).
        let (target, forward) = match &path.look_at {
            LookAt::Heading => {
                let heading = path.heading(local);
                (eye + heading * HEADING_REACH, heading)
            }
            _ => {
                let target = path.target(local, eye);
                (target, target - eye)
            }
        };
        let roll_degrees = shot.camera.roll.sample_or(local, 0.0);
        let (rotation, up) = match sample_rotation(&shot.camera.rotation, local) {
            Some(keyed) => roll_rotation(keyed, roll_degrees),
            None => look_rotation_along(forward, Vec3::Y, roll_degrees),
        };
        let fov_y_degrees = shot.camera.fov.sample_or(local, DEFAULT_FOV_DEGREES);
        let dof = shot.camera.dof.as_ref().map(|dof| DofSample {
            focus_distance: if dof.focus_on_target {
                eye.distance(target).max(1.0e-3)
            } else {
                dof.focus_distance.sample_or(local, DEFAULT_FOCUS_DISTANCE)
            },
            aperture: dof.aperture.sample_or(local, DEFAULT_APERTURE),
            bokeh: dof.bokeh,
        });
        Some(ShotSample {
            time,
            shot: index,
            local_time: local,
            eye,
            target,
            up,
            rotation,
            roll_degrees,
            fov_y_degrees,
            dof,
            fade: fade_amount(shot, local),
            fade_color: shot.fade_color,
            finished: time >= total,
        })
    }

    /// [`Self::sample`] at frame `frame` of this sequence's authoring frame rate.
    #[must_use]
    pub fn sample_frame(&self, frame: u64) -> Option<ShotSample> {
        self.sample(time_of_frame(frame, self.fps))
    }
}

#[cfg(test)]
mod tests {
    use super::super::asset::fixtures::{key, sequence, shot};
    use super::super::asset::{DepthOfFieldTracks, ScalarKey};
    use super::*;

    fn track(keys: &[(f32, f32, Ease)]) -> Track {
        Track(
            keys.iter()
                .map(|&(time, value, ease)| ScalarKey { time, value, ease })
                .collect(),
        )
    }

    #[test]
    fn tracks_hold_at_the_ends_and_ease_between_keys() {
        let t = track(&[
            (1.0, 10.0, Ease::Linear),
            (3.0, 30.0, Ease::Smooth),
            (5.0, 50.0, Ease::Step),
        ]);
        assert_eq!(t.sample(0.0), Some(10.0), "before the first key");
        assert_eq!(t.sample(1.0), Some(10.0));
        assert_eq!(t.sample(2.0), Some(20.0), "linear midpoint");
        assert_eq!(t.sample(3.0), Some(30.0));
        assert_eq!(t.sample(4.0), Some(40.0), "smoothstep is 0.5 at the midpoint");
        assert_eq!(t.sample(5.0), Some(50.0));
        assert_eq!(t.sample(9.0), Some(50.0), "after the last key");
        assert_eq!(Track::default().sample(1.0), None);
        assert_eq!(Track::default().sample_or(1.0, 7.0), 7.0);
    }

    #[test]
    fn a_step_key_holds_its_value_until_the_next_key() {
        let t = track(&[
            (0.0, 1.0, Ease::Step),
            (2.0, 5.0, Ease::Linear),
            (4.0, 9.0, Ease::Linear),
        ]);
        assert_eq!(t.sample(1.999), Some(1.0));
        assert_eq!(t.sample(2.0), Some(5.0));
        assert_eq!(t.sample(3.0), Some(7.0));
    }

    #[test]
    fn linear_paths_interpolate_and_hold_at_the_ends() {
        let s = sequence(vec![shot("a", 4.0)]);
        let at = |t: f64| s.sample(t).expect("sample").eye;
        assert_eq!(at(0.0), Vec3::new(0.0, 0.0, 0.0));
        assert!(at(1.0).abs_diff_eq(Vec3::new(2.5, 0.0, 0.0), 1.0e-5));
        assert_eq!(at(4.0), Vec3::new(10.0, 0.0, 0.0));
        assert_eq!(at(100.0), Vec3::new(10.0, 0.0, 0.0), "held past the end");
        assert_eq!(at(-5.0), Vec3::ZERO, "clamped before the start");
        assert_eq!(at(f64::NAN), Vec3::ZERO, "a NaN time reads as zero");
    }

    #[test]
    fn catmull_rom_paths_pass_through_their_keys_at_the_key_times() {
        let mut s = sequence(vec![shot("a", 3.0)]);
        let path = &mut s.shots[0].camera.path;
        path.interp = PathInterp::CatmullRom;
        path.keys = vec![
            PathKey {
                time: 0.0,
                position: [0.0, 0.0, 0.0],
            },
            PathKey {
                time: 1.0,
                position: [4.0, 1.0, 0.0],
            },
            PathKey {
                time: 2.0,
                position: [4.0, 2.0, 5.0],
            },
            PathKey {
                time: 3.0,
                position: [0.0, 3.0, 5.0],
            },
        ];
        s.validate().expect("valid");
        for k in s.shots[0].camera.path.keys.clone() {
            let eye = s.sample(f64::from(k.time)).expect("sample").eye;
            assert!(
                eye.abs_diff_eq(Vec3::from_array(k.position), 1.0e-4),
                "{eye:?} vs {:?}",
                k.position
            );
        }
        let mid = s.sample(1.5).expect("sample").eye;
        assert!(mid.is_finite() && mid.y > 1.0 && mid.y < 2.0, "{mid:?}");
    }

    #[test]
    fn heading_looks_along_the_direction_of_travel() {
        let mut s = sequence(vec![shot("a", 2.0)]);
        s.shots[0].camera.path.look_at = LookAt::Heading;
        let sample = s.sample(1.0).expect("sample");
        assert!(
            sample
                .target
                .abs_diff_eq(sample.eye + Vec3::X * HEADING_REACH, 1.0e-3),
            "{sample:?}"
        );
        // Facing +X: the rotation sends −Z to +X.
        assert!((sample.rotation * Vec3::NEG_Z).abs_diff_eq(Vec3::X, 1.0e-4));
        assert!((sample.rotation * Vec3::Y).abs_diff_eq(Vec3::Y, 1.0e-4));
    }

    /// The s4 showcase truck (ADR-1488): ~1.5 m/s, 2 km from the origin.
    fn s4_keys() -> Vec<PathKey> {
        [
            (0.0, [-1752.0, 353.2, -2203.0]),
            (6.5, [-1758.69, 354.7, -2195.57]),
            (13.0, [-1765.38, 356.2, -2188.14]),
            (19.5, [-1772.07, 357.7, -2180.71]),
            (26.0, [-1778.77, 359.2, -2173.28]),
        ]
        .into_iter()
        .map(|(time, position)| PathKey { time, position })
        .collect()
    }

    /// Angle between two directions, radians (f64 cross product: exact for small angles).
    fn angle(a: Vec3, b: Vec3) -> f64 {
        let (a, b) = (a.as_dvec3().normalize(), b.as_dvec3().normalize());
        a.cross(b).length().atan2(a.dot(b))
    }

    #[test]
    fn heading_shots_far_from_the_origin_look_along_their_travel_smoothly() {
        // In f32 the ±1/120 s difference of positions rounded to 1.2e-4..2.4e-4 m (about 2.6 cm
        // apart) swung the view by up to 0.05 rad a frame; the point 10 m ahead then quantized
        // what was left to 2.4e-5 rad. In f64 the view follows the path's own gentle curvature:
        // its keys are not quite collinear, so the heading turns up to 3.8e-6 rad a frame, and
        // the turn changes by up to 1.5e-6 rad a frame where the curvature jumps at a key
        // (Catmull-Rom is C1), plus f32 rounding of the rotation (a few 1e-7 rad).
        let mut s = sequence(vec![shot("a", 26.0)]);
        let path = &mut s.shots[0].camera.path;
        path.interp = PathInterp::CatmullRom;
        path.keys = s4_keys();
        path.look_at = LookAt::Heading;
        s.validate().expect("valid");
        let travel =
            (Vec3::from_array(s4_keys()[4].position) - Vec3::from_array(s4_keys()[0].position)).normalize();
        let forwards: Vec<Vec3> = (1..1559u32)
            .map(|frame| s.sample(f64::from(frame) / 60.0).expect("sample").rotation * Vec3::NEG_Z)
            .collect();
        for (i, forward) in forwards.iter().enumerate() {
            assert!(
                angle(*forward, travel) < 2.0e-3,
                "frame {}: {forward:?} vs {travel:?}",
                i + 1
            );
        }
        let turns: Vec<f64> = forwards.windows(2).map(|w| angle(w[0], w[1])).collect();
        for (i, turn) in turns.iter().enumerate() {
            assert!(*turn < 1.0e-5, "frame {}: the view turned {turn:e} rad", i + 2);
        }
        for (i, pair) in turns.windows(2).enumerate() {
            let change = (pair[1] - pair[0]).abs();
            assert!(
                change < 4.0e-6,
                "frame {}: turn {:e} then {:e} rad",
                i + 3,
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn a_far_look_at_point_turns_the_view_by_an_even_step() {
        // The s4 shot itself: a lateral truck looking at a point 3.3 km away turns at ~7.8e-6
        // rad a frame. The sampled rotation turns by that step every frame, within f32 rounding
        // of the quaternion (the 1.8e-4 rad ticks of the render came later, in the renderer's
        // extraction: `tests/camera_precision.rs`). A guard for this half of the pipeline.
        let mut s = sequence(vec![shot("a", 26.0)]);
        let path = &mut s.shots[0].camera.path;
        path.interp = PathInterp::CatmullRom;
        path.keys = s4_keys();
        path.look_at = LookAt::Point([700.0, 530.0, 5.0]);
        s.validate().expect("valid");
        let forwards: Vec<Vec3> = (0..1560u32)
            .map(|frame| s.sample(f64::from(frame) / 60.0).expect("sample").rotation * Vec3::NEG_Z)
            .collect();
        let turns: Vec<f64> = forwards.windows(2).map(|w| angle(w[0], w[1])).collect();
        for (i, pair) in turns.windows(2).enumerate() {
            let change = (pair[1] - pair[0]).abs();
            assert!(
                change < 3.0e-6,
                "frame {}: turn {:e} then {:e} rad",
                i + 2,
                pair[0],
                pair[1]
            );
        }
        let mean = turns.iter().sum::<f64>() / turns.len() as f64;
        assert!((mean - 7.8e-6).abs() < 0.5e-6, "mean turn {mean:e} rad a frame");
    }

    #[test]
    fn look_rotation_along_matches_look_rotation() {
        let eye = Vec3::new(1.0, 2.0, 3.0);
        for target in [Vec3::new(4.0, 0.0, -2.0), Vec3::new(1.0, 9.0, 3.0), eye] {
            assert_eq!(
                look_rotation(eye, target, Vec3::Y, 12.0),
                look_rotation_along(target - eye, Vec3::Y, 12.0)
            );
        }
        let (q, _) = look_rotation_along(Vec3::X * 1.0e-9, Vec3::Y, 0.0);
        assert!(
            (q * Vec3::NEG_Z).abs_diff_eq(Vec3::NEG_Z, 1.0e-6),
            "a zero direction looks along −Z"
        );
    }

    #[test]
    fn a_still_path_with_heading_does_not_produce_nan() {
        let mut s = sequence(vec![shot("a", 2.0)]);
        s.shots[0].camera.path.keys = vec![key(0.0, 3.0)];
        s.shots[0].camera.path.look_at = LookAt::Heading;
        let sample = s.sample(1.0).expect("sample");
        assert!(sample.rotation.is_finite() && sample.target.is_finite());
        assert!(
            (sample.rotation * Vec3::NEG_Z).abs_diff_eq(Vec3::NEG_Z, 1.0e-4),
            "falls back to −Z"
        );
    }

    #[test]
    fn look_at_a_point_faces_it_and_a_moving_target_follows() {
        let mut s = sequence(vec![shot("a", 2.0)]);
        s.shots[0].camera.path.look_at = LookAt::Track(vec![
            PathKey {
                time: 0.0,
                position: [0.0, 0.0, -5.0],
            },
            PathKey {
                time: 2.0,
                position: [10.0, 0.0, -5.0],
            },
        ]);
        let at = s.sample(1.0).expect("sample");
        assert!(at.target.abs_diff_eq(Vec3::new(5.0, 0.0, -5.0), 1.0e-4));
        let forward = at.rotation * Vec3::NEG_Z;
        assert!(
            forward.abs_diff_eq((at.target - at.eye).normalize(), 1.0e-4),
            "{forward:?}"
        );
    }

    #[test]
    fn roll_tilts_the_up_vector_toward_the_right() {
        let (q, up) = look_rotation(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y, 90.0);
        assert!(up.abs_diff_eq(Vec3::X, 1.0e-5), "{up:?}");
        assert!((q * Vec3::Y).abs_diff_eq(Vec3::X, 1.0e-5));
        assert!((q * Vec3::NEG_Z).abs_diff_eq(Vec3::NEG_Z, 1.0e-5));
        let (_, level) = look_rotation(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y, 0.0);
        assert!(level.abs_diff_eq(Vec3::Y, 1.0e-5));
    }

    #[test]
    fn looking_straight_up_or_at_the_eye_stays_finite() {
        for target in [Vec3::Y * 5.0, Vec3::NEG_Y * 5.0, Vec3::ZERO] {
            let (q, up) = look_rotation(Vec3::ZERO, target, Vec3::Y, 0.0);
            assert!(q.is_finite() && up.is_finite(), "{target:?}");
            assert!((q.length() - 1.0).abs() < 1.0e-5);
        }
    }

    #[test]
    fn cuts_land_exactly_on_shot_boundaries() {
        let s = sequence(vec![shot("a", 2.0), shot("b", 3.0), shot("c", 1.0)]);
        assert_eq!(s.duration(), 6.0);
        assert_eq!(s.locate(0.0), Some((0, 0.0)));
        assert_eq!(s.locate(1.999), Some((0, 1.999)));
        assert_eq!(
            s.locate(2.0),
            Some((1, 0.0)),
            "the boundary belongs to the next shot"
        );
        assert_eq!(s.locate(4.999), Some((1, 2.999)));
        assert_eq!(s.locate(5.0), Some((2, 0.0)));
        assert_eq!(
            s.locate(6.0),
            Some((2, 1.0)),
            "the end holds the last frame of the last shot"
        );
        assert_eq!(s.locate(60.0), Some((2, 1.0)));
        assert_eq!(s.locate(-1.0), Some((0, 0.0)));
        assert_eq!(s.shot_start(2), 5.0);
        assert_eq!(s.shot_index("b"), Some(1));
        assert_eq!(s.shot_index("nope"), None);
        assert!(!s.sample(5.999).expect("sample").finished);
        assert!(s.sample(6.0).expect("sample").finished);
        assert!(ShotSequence::default().sample(0.0).is_none());
        assert!(ShotSequence::default().locate(0.0).is_none());
    }

    #[test]
    fn each_shot_has_its_own_clock() {
        let s = sequence(vec![shot("a", 2.0), shot("b", 2.0)]);
        let first = s.sample(0.5).expect("sample");
        let second = s.sample(2.5).expect("sample");
        assert_eq!((first.shot, second.shot), (0, 1));
        assert_eq!(first.local_time, 0.5);
        assert_eq!(second.local_time, 0.5);
        assert_eq!(first.eye, second.eye, "both shots run the same move");
    }

    #[test]
    fn fades_ramp_at_the_shot_edges_and_are_clear_between() {
        let mut s = sequence(vec![shot("a", 4.0)]);
        s.shots[0].fade_in = 1.0;
        s.shots[0].fade_out = 2.0;
        s.shots[0].fade_color = [0.1, 0.2, 0.3];
        let fade = |t: f64| s.sample(t).expect("sample").fade;
        assert_eq!(fade(0.0), 1.0, "starts fully faded");
        assert_eq!(fade(0.5), 0.5);
        assert_eq!(fade(1.0), 0.0, "the ramp ends exactly at fade_in");
        assert_eq!(fade(2.0), 0.0, "fade_out starts at duration − fade_out");
        assert_eq!(fade(3.0), 0.5);
        assert_eq!(fade(4.0), 1.0, "ends fully faded");
        assert_eq!(s.sample(1.0).expect("sample").fade_color, [0.1, 0.2, 0.3]);
    }

    #[test]
    fn a_fade_through_black_spans_a_cut() {
        let mut s = sequence(vec![shot("a", 2.0), shot("b", 2.0)]);
        s.shots[0].fade_out = 0.5;
        s.shots[1].fade_in = 0.5;
        let near_cut_before = s.sample(1.999).expect("sample");
        let at_cut = s.sample(2.0).expect("sample");
        assert!(
            near_cut_before.fade > 0.99 && at_cut.fade == 1.0,
            "black on both sides of the cut"
        );
        assert_eq!((near_cut_before.shot, at_cut.shot), (0, 1));
    }

    #[test]
    fn the_lens_follows_its_tracks_and_defaults_when_empty() {
        let mut s = sequence(vec![shot("a", 2.0)]);
        assert_eq!(s.sample(1.0).expect("sample").fov_y_degrees, DEFAULT_FOV_DEGREES);
        assert!(s.sample(1.0).expect("sample").dof.is_none());
        s.shots[0].camera.fov = track(&[(0.0, 20.0, Ease::Linear), (2.0, 60.0, Ease::Linear)]);
        s.shots[0].camera.roll = track(&[(0.0, 0.0, Ease::Linear), (2.0, 10.0, Ease::Linear)]);
        s.shots[0].camera.dof = Some(DepthOfFieldTracks {
            focus_distance: track(&[(0.0, 2.0, Ease::Linear), (2.0, 6.0, Ease::Linear)]),
            aperture: track(&[(0.0, 1.4, Ease::Step)]),
            focus_on_target: false,
            bokeh: true,
        });
        let sample = s.sample(1.0).expect("sample");
        assert_eq!(sample.fov_y_degrees, 40.0);
        assert_eq!(sample.roll_degrees, 5.0);
        let dof = sample.dof.expect("dof");
        assert_eq!((dof.focus_distance, dof.aperture, dof.bokeh), (4.0, 1.4, true));
    }

    #[test]
    fn focus_on_target_tracks_the_eye_to_target_distance() {
        let mut s = sequence(vec![shot("a", 2.0)]);
        s.shots[0].camera.path.look_at = LookAt::Point([0.0, 0.0, 0.0]);
        s.shots[0].camera.dof = Some(DepthOfFieldTracks {
            focus_on_target: true,
            ..DepthOfFieldTracks::default()
        });
        let sample = s.sample(1.0).expect("sample");
        let dof = sample.dof.expect("dof");
        assert!((dof.focus_distance - sample.eye.distance(Vec3::ZERO)).abs() < 1.0e-5);
        assert_eq!(dof.aperture, DEFAULT_APERTURE);
    }

    #[test]
    fn sampling_is_deterministic_and_independent_of_the_step() {
        let mut s = sequence(vec![shot("a", 3.0), shot("b", 2.0)]);
        s.shots[0].camera.path.interp = PathInterp::CatmullRom;
        s.shots[0].camera.path.keys = vec![
            PathKey {
                time: 0.0,
                position: [0.0, 1.0, 0.0],
            },
            PathKey {
                time: 1.0,
                position: [3.0, 2.0, 1.0],
            },
            PathKey {
                time: 3.0,
                position: [6.0, 1.0, 4.0],
            },
        ];
        s.shots[0].fade_out = 1.0;
        s.validate().expect("valid");
        // Sampling the same instants through two different "frame rates" and in a different
        // order gives bit-identical samples.
        let times: Vec<f64> = (0..=150).map(|i| f64::from(i) / 30.0).collect();
        let forward: Vec<ShotSample> = times.iter().map(|&t| s.sample(t).expect("sample")).collect();
        let backward: Vec<ShotSample> = times
            .iter()
            .rev()
            .map(|&t| s.sample(t).expect("sample"))
            .collect();
        for (a, b) in forward.iter().zip(backward.iter().rev()) {
            assert_eq!(a, b, "time {}", a.time);
        }
        let twin = s.clone();
        assert_eq!(s.sample(1.234_567), twin.sample(1.234_567));
        assert_eq!(s.sample_frame(45), s.sample(1.5));
        assert_eq!(time_of_frame(45, 30.0), 1.5);
    }

    fn rot_key(time: f32, q: Quat, ease: Ease) -> RotationKey {
        RotationKey {
            time,
            rotation: q.to_array(),
            ease,
        }
    }

    #[test]
    fn rotation_keys_slerp_along_the_shortest_arc() {
        let a = Quat::IDENTITY;
        let b = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        let keys = [rot_key(0.0, a, Ease::Linear), rot_key(2.0, b, Ease::Linear)];
        assert!(sample_rotation(&[], 1.0).is_none());
        assert!(sample_rotation(&keys, -1.0).expect("held").abs_diff_eq(a, 1.0e-6));
        assert!(sample_rotation(&keys, 9.0).expect("held").abs_diff_eq(b, 1.0e-6));
        let mid = sample_rotation(&keys, 1.0).expect("mid");
        let want = Quat::from_rotation_y(std::f32::consts::FRAC_PI_4);
        assert!(mid.abs_diff_eq(want, 1.0e-5), "{mid:?} vs {want:?}");
        // Constant angular speed: a quarter of the way is a quarter of the angle.
        let quarter = sample_rotation(&keys, 0.5).expect("quarter");
        assert!((quarter.angle_between(a) - std::f32::consts::FRAC_PI_8).abs() < 1.0e-5);
    }

    #[test]
    fn rotation_slerp_takes_the_short_way_for_a_flipped_sign() {
        let a = Quat::from_rotation_y(0.1);
        let b = -Quat::from_rotation_y(0.3); // the same orientation as +0.3 rad
        let keys = [rot_key(0.0, a, Ease::Linear), rot_key(1.0, b, Ease::Linear)];
        let mid = sample_rotation(&keys, 0.5).expect("mid");
        let want = Quat::from_rotation_y(0.2);
        assert!(
            mid.abs_diff_eq(want, 1.0e-5) || mid.abs_diff_eq(-want, 1.0e-5),
            "{mid:?} vs {want:?}"
        );
    }

    #[test]
    fn rotation_ease_smooth_and_step() {
        let a = Quat::IDENTITY;
        let b = Quat::from_rotation_z(1.0);
        let smooth = [rot_key(0.0, a, Ease::Smooth), rot_key(2.0, b, Ease::Linear)];
        let at = |t: f32| sample_rotation(&smooth, t).expect("sample").angle_between(a);
        assert!(
            (at(1.0) - 0.5).abs() < 1.0e-5,
            "smoothstep is half way at the midpoint"
        );
        assert!(
            at(0.5) < 0.25,
            "smoothstep starts slower than linear: {}",
            at(0.5)
        );
        let step = [rot_key(0.0, a, Ease::Step), rot_key(2.0, b, Ease::Linear)];
        assert!(
            sample_rotation(&step, 1.999)
                .expect("held")
                .abs_diff_eq(a, 1.0e-6)
        );
        assert!(sample_rotation(&step, 2.0).expect("next").abs_diff_eq(b, 1.0e-6));
    }

    #[test]
    fn a_degenerate_key_reads_as_the_identity_not_nan() {
        let bad = RotationKey {
            time: 0.0,
            rotation: [0.0; 4],
            ease: Ease::Linear,
        };
        let q = sample_rotation(&[bad], 0.0).expect("held");
        assert!(q.is_finite() && q.abs_diff_eq(Quat::IDENTITY, 1.0e-6));
    }

    #[test]
    fn rotation_keys_replace_look_at_and_roll_applies_on_top() {
        let mut s = sequence(vec![shot("a", 4.0)]);
        let yaw = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        s.shots[0].camera.rotation = vec![
            rot_key(0.0, Quat::IDENTITY, Ease::Linear),
            rot_key(4.0, yaw, Ease::Linear),
        ];
        s.validate().expect("valid");
        let end = s.sample(4.0).expect("end");
        assert!(
            end.rotation.abs_diff_eq(yaw, 1.0e-5),
            "keyed, not the look-at orientation"
        );
        assert!(end.up.abs_diff_eq(Vec3::Y, 1.0e-5));
        // The look-at point still reports the target (for focus_on_target).
        assert_eq!(end.target, Vec3::new(0.0, 0.0, -10.0));
        // Roll turns the camera about its own forward axis.
        s.shots[0].camera.roll = Track(vec![ScalarKey {
            time: 0.0,
            value: 90.0,
            ease: Ease::Linear,
        }]);
        let rolled = s.sample(0.0).expect("start");
        let forward = rolled.rotation * Vec3::NEG_Z;
        assert!(
            forward.abs_diff_eq(Vec3::NEG_Z, 1.0e-5),
            "roll keeps the view direction"
        );
        assert!(rolled.up.abs_diff_eq(Vec3::X, 1.0e-5) || rolled.up.abs_diff_eq(-Vec3::X, 1.0e-5));
        assert!((rolled.up.dot(Vec3::Y)).abs() < 1.0e-5);
        // Without keys the old behavior is unchanged.
        s.shots[0].camera.rotation.clear();
        let looked = s.sample(0.0).expect("look-at");
        let toward_target = (looked.target - looked.eye).normalize();
        assert!(
            (looked.rotation * Vec3::NEG_Z).abs_diff_eq(toward_target, 1.0e-5),
            "look-at drives the view again"
        );
    }

    #[test]
    fn rotation_sampling_is_deterministic() {
        let mut s = sequence(vec![shot("a", 3.0)]);
        s.shots[0].camera.rotation = vec![
            rot_key(0.0, Quat::from_rotation_x(0.3), Ease::Smooth),
            rot_key(
                1.5,
                Quat::from_rotation_y(0.4) * Quat::from_rotation_x(-0.7),
                Ease::Linear,
            ),
            rot_key(3.0, Quat::from_rotation_z(-1.1), Ease::Linear),
        ];
        let run = || (0..90).map(|f| s.sample_frame(f)).collect::<Vec<_>>();
        assert_eq!(run(), run());
    }

    #[test]
    fn nan_key_times_hold_the_first_key_instead_of_aborting() {
        let nan = f32::NAN;
        let single = [key(nan, 3.0)];
        assert_eq!(
            sample_keys(&single, PathInterp::Linear, 0.5),
            vec3(single[0].position)
        );
        let two = [key(nan, 1.0), key(2.0, 5.0)];
        let _ = sample_keys(&two, PathInterp::Linear, 0.5);
        let t = track(&[(nan, 4.0, Ease::Linear)]);
        assert_eq!(t.sample(0.5), Some(4.0));
        let rot = [RotationKey {
            time: nan,
            rotation: [0.0, 0.0, 0.0, 1.0],
            ease: Ease::Linear,
        }];
        assert!(sample_rotation(&rot, 0.5).is_some());
    }

    #[test]
    fn locate_refuses_nan_or_negative_durations() {
        for bad in [f32::NAN, -1.0] {
            let seq = sequence(vec![shot("a", bad)]);
            assert_eq!(seq.locate(0.0), None);
            assert!(seq.sample(1.0).is_none());
        }
        let seq = sequence(vec![shot("a", 2.0), shot("b", -1.0)]);
        assert_eq!(seq.locate(0.5), None);
    }
}
