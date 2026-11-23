// SPDX-License-Identifier: MPL-2.0
//! Procedural locomotion content for motion matching: a three-joint rig (a root that carries the
//! root motion and two feet), clips made in code (idle, walk, run, a left and a right turn, a
//! stop), the `.motiondb.ron` source that binds them, and the matching desired paths.
//!
//! Nothing here reads a file: the sample (`samples/mm_locomotion`), the cook tests and the agent
//! dogfood test all use the same content, so a cooked database and a runtime-built one can be
//! compared on equal inputs. The paths the source names (`procedural/locomotion#...`) are virtual:
//! [`ProceduralLocomotion::clip`] answers them.
//!
//! Each clip moves the root at `speed` m/s while turning at `turn` rad/s (toward +X for a positive
//! turn), keyed at 30 Hz. The feet swing `stride` meters along the heading with period `period`
//! and opposite phase, and lift on their forward swing. The stop clip decelerates linearly from
//! walking speed to rest over 0.8 s, then stands for 0.4 s.

// Source: the circular-walk test content of `docs/specs/52-motion-matching.md` § 8 (root on a
// circle of radius `v / omega`, feet oscillating along the heading). Original code.

use std::f32::consts::TAU;

use regia_core::math::{Quat, Vec2, Vec3};

use super::asset::{EntrySource, MotionDatabaseSource, SchemaSource};
use super::{MotionDatabase, MotionMatchError, TrajectoryPoint};
use crate::clip::{AnimationClip, JointTrack, TrackCurve};
use crate::num::{det_rotation_y, det_sin_cos};
use crate::pose::translated;
use crate::skeleton::Skeleton;

/// The skeleton's asset path in [`ProceduralLocomotion::source`].
pub const SKELETON_PATH: &str = "procedural/locomotion#skeleton/root";

/// Key rate of the procedural clips, keys per second.
const KEY_RATE: f32 = 30.0;

/// One procedural locomotion clip.
#[derive(Clone, Debug, PartialEq)]
pub struct LocomotionSpec {
    /// Clip name (also the last part of its asset path).
    pub name: &'static str,
    /// Ground speed, meters per second.
    pub speed: f32,
    /// Turn rate, radians per second (positive toward +X).
    pub turn: f32,
    /// Foot swing amplitude, meters.
    pub stride: f32,
    /// Step period, seconds.
    pub period: f32,
    /// Clip length, seconds.
    pub duration: f32,
    /// The clip loops.
    pub looping: bool,
    /// Tag names of every frame.
    pub tags: &'static [&'static str],
    /// Decelerate to rest over this many seconds (a stop clip); `None` keeps `speed`.
    pub stop_over: Option<f32>,
}

impl LocomotionSpec {
    /// The clip's asset path in [`ProceduralLocomotion::source`].
    pub fn path(&self) -> String {
        format!("procedural/locomotion#clip/{}", self.name)
    }
}

/// The six clips of the locomotion set, in database entry order.
pub fn locomotion_specs() -> Vec<LocomotionSpec> {
    let walk = |name, turn, tags| LocomotionSpec {
        name,
        speed: 1.5,
        turn,
        stride: 0.3,
        period: 1.0,
        duration: 2.0,
        looping: true,
        tags,
        stop_over: None,
    };
    vec![
        LocomotionSpec {
            name: "idle",
            speed: 0.0,
            turn: 0.0,
            stride: 0.0,
            period: 2.0,
            duration: 2.0,
            looping: true,
            tags: &["idle"],
            stop_over: None,
        },
        walk("walk", 0.0, &["walk"]),
        LocomotionSpec {
            name: "run",
            speed: 4.0,
            turn: 0.0,
            stride: 0.6,
            period: 0.6,
            duration: 1.2,
            looping: true,
            tags: &["run"],
            stop_over: None,
        },
        walk("turn_left", -1.2, &["walk", "turn"]),
        walk("turn_right", 1.2, &["walk", "turn"]),
        LocomotionSpec {
            name: "stop",
            speed: 1.5,
            turn: 0.0,
            stride: 0.3,
            period: 1.0,
            duration: 1.2,
            looping: false,
            tags: &["stop"],
            stop_over: Some(0.8),
        },
    ]
}

/// The root (joint 0, no parent) with the feet (joints 1 and 2) hanging 0.1 m to each side.
pub fn locomotion_skeleton() -> Skeleton {
    // A fixed three-joint chain always validates; the fallback is never taken.
    Skeleton::from_local_bind(
        vec!["root".into(), "foot_l".into(), "foot_r".into()],
        vec![None, Some(0), Some(0)],
        vec![
            translated(Vec3::ZERO),
            translated(Vec3::new(-0.1, 0.0, 0.0)),
            translated(Vec3::new(0.1, 0.0, 0.0)),
        ],
    )
    .unwrap_or_default()
}

/// Distance travelled and heading after `t` seconds of `spec` (heading 0 = +Z).
fn travel(spec: &LocomotionSpec, t: f32) -> (f32, f32) {
    let distance = match spec.stop_over {
        Some(over) if over > 0.0 => {
            let s = t.min(over);
            spec.speed * (s - s * s / (2.0 * over))
        }
        _ => spec.speed * t,
    };
    let heading = match spec.stop_over {
        Some(over) if over > 0.0 => spec.turn * t.min(over),
        _ => spec.turn * t,
    };
    (distance, heading)
}

/// The clip of `spec`: root translation and yaw keys, foot swing keys.
pub fn locomotion_clip(spec: &LocomotionSpec) -> AnimationClip {
    let keys = (spec.duration * KEY_RATE).round() as usize;
    let times: Vec<f32> = (0..=keys)
        .map(|k| (k as f32 / KEY_RATE).min(spec.duration))
        .collect();
    let root_at = |t: f32| {
        let (distance, heading) = travel(spec, t);
        if spec.turn.abs() < 1e-6 || spec.speed.abs() < 1e-6 {
            Vec3::new(0.0, 0.0, distance)
        } else {
            // A constant-speed arc of radius v / omega (stop clips never turn).
            let r = spec.speed / spec.turn;
            let (sin, cos) = det_sin_cos(heading);
            Vec3::new(r * (1.0 - cos), 0.0, r * sin)
        }
    };
    let amplitude = |t: f32| match spec.stop_over {
        Some(over) if over > 0.0 => (1.0 - t / over).max(0.0),
        _ => 1.0,
    };
    let phase = |t: f32| det_sin_cos(TAU * t / spec.period).0 * amplitude(t);
    let mut clip = AnimationClip::empty(spec.name, spec.duration);
    clip.sample_rate = KEY_RATE;
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: times.clone(),
            values: times.iter().map(|&t| root_at(t)).collect(),
            exact_rotation: false,
        },
        rotations: TrackCurve::Animated {
            times: times.clone(),
            values: times.iter().map(|&t| det_rotation_y(travel(spec, t).1)).collect(),
            exact_rotation: true,
        },
        scales: TrackCurve::Static(Vec3::ONE),
    });
    for (joint, side) in [(1u32, 1.0f32), (2u32, -1.0f32)] {
        clip.tracks.push(JointTrack {
            joint,
            translations: TrackCurve::Animated {
                times: times.clone(),
                values: times
                    .iter()
                    .map(|&t| {
                        let s = side * phase(t);
                        Vec3::new(-0.1 * side, 0.05 * s.max(0.0), spec.stride * s)
                    })
                    .collect(),
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
    }
    clip
}

/// The desired path of moving at `speed` m/s while turning at `turn` rad/s, sampled at `times`
/// seconds ahead, in the character frame (`x` right, `y` forward).
pub fn arc_trajectory(times: &[f32], speed: f32, turn: f32) -> Vec<TrajectoryPoint> {
    times
        .iter()
        .map(|&t| {
            let heading = turn * t;
            let (sin, cos) = det_sin_cos(heading);
            let position = if turn.abs() < 1e-6 {
                Vec2::new(0.0, speed * t)
            } else {
                let r = speed / turn;
                Vec2::new(r * (1.0 - cos), r * sin)
            };
            TrajectoryPoint {
                position,
                direction: Vec2::new(sin, cos),
            }
        })
        .collect()
}

/// The whole procedural locomotion set: rig, clips by path and the database source.
#[derive(Clone, Debug)]
pub struct ProceduralLocomotion {
    /// The rig.
    pub skeleton: Skeleton,
    /// `(asset path, clip)` in entry order.
    pub clips: Vec<(String, AnimationClip)>,
    /// The `.motiondb.ron` source naming them.
    pub source: MotionDatabaseSource,
}

impl ProceduralLocomotion {
    /// The clip at an asset path of [`ProceduralLocomotion::source`].
    pub fn clip(&self, path: &str) -> Option<&AnimationClip> {
        self.clips.iter().find(|(p, _)| p == path).map(|(_, c)| c)
    }

    /// The clips in entry order (what a [`super::player::MotionMatchPlayer`] takes).
    pub fn entry_clips(&self) -> Vec<AnimationClip> {
        self.clips.iter().map(|(_, c)| c.clone()).collect()
    }

    /// Build the database at runtime from the source (the same call a cook makes).
    ///
    /// # Errors
    ///
    /// The errors of [`MotionDatabaseSource::build`].
    pub fn build(&self) -> Result<MotionDatabase, MotionMatchError> {
        self.source.build(&self.skeleton, |path| self.clip(path))
    }
}

/// The procedural locomotion set: six clips, tags `idle`, `walk`, `run`, `turn`, `stop`, the
/// default schema on the root and both feet.
pub fn procedural_locomotion() -> ProceduralLocomotion {
    let specs = locomotion_specs();
    let clips: Vec<(String, AnimationClip)> = specs
        .iter()
        .map(|spec| (spec.path(), locomotion_clip(spec)))
        .collect();
    let source = MotionDatabaseSource {
        format_version: super::asset::MOTIONDB_FORMAT_VERSION,
        skeleton: SKELETON_PATH.into(),
        schema: SchemaSource {
            root_joint: "root".into(),
            forward: [0.0, 0.0, 1.0],
            feet: vec!["foot_l".into(), "foot_r".into()],
            trajectory_times: super::DEFAULT_TRAJECTORY_TIMES.to_vec(),
            history_times: Vec::new(),
            sample_rate: super::DEFAULT_SAMPLE_RATE,
            weights: Default::default(),
            normalization: Default::default(),
            pose_joints: Vec::new(),
            sample_weights: Default::default(),
            contacts: None,
        },
        tags: ["idle", "walk", "run", "turn", "stop"]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        mirror_pairs: Vec::new(),
        entries: specs
            .iter()
            .map(|spec| EntrySource {
                clip: spec.path(),
                looping: spec.looping,
                tags: spec.tags.iter().map(|s| (*s).to_string()).collect(),
                mirrored: false,
                trim: (0.0, 0.0),
                spans: Vec::new(),
                distance_matched: spec.stop_over.is_some(),
            })
            .collect(),
    };
    ProceduralLocomotion {
        skeleton: locomotion_skeleton(),
        clips,
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_set_builds_and_every_clip_is_finite() {
        let set = procedural_locomotion();
        let database = set.build().expect("database");
        assert_eq!(database.clips().len(), 6);
        for (path, clip) in &set.clips {
            assert!(clip.duration > 0.0, "{path}");
            for track in &clip.tracks {
                if let TrackCurve::Animated { values, .. } = &track.translations {
                    assert!(values.iter().all(|v| v.is_finite()), "{path}");
                }
            }
        }
    }

    #[test]
    fn a_turning_clip_ends_where_its_arc_says() {
        let spec = locomotion_specs()
            .into_iter()
            .find(|s| s.name == "turn_right")
            .expect("turn_right");
        let clip = locomotion_clip(&spec);
        let TrackCurve::Animated { values, .. } = &clip.tracks[0].translations else {
            panic!("animated root");
        };
        let end = values.last().copied().expect("keys");
        let path = arc_trajectory(&[spec.duration], spec.speed, spec.turn);
        assert!(
            (end.x - path[0].position.x).abs() < 1e-4,
            "{end} vs {:?}",
            path[0]
        );
        assert!(
            (end.z - path[0].position.y).abs() < 1e-4,
            "{end} vs {:?}",
            path[0]
        );
    }

    /// ADR-4477 / audit animation1-determinism L9: procedural turning keys use the same
    /// portable series as runtime extraction and prediction.
    #[test]
    fn procedural_turn_keys_use_the_portable_trig_oracle() {
        let angle = f32::from_bits(0x3edf_285b);
        let spec = LocomotionSpec {
            name: "portable_turn",
            speed: 1.0,
            turn: angle,
            stride: 0.0,
            period: 1.0,
            duration: 1.0,
            looping: true,
            tags: &[],
            stop_over: None,
        };
        let clip = locomotion_clip(&spec);
        let (sin, cos) = det_sin_cos(angle);
        let radius = 1.0 / angle;
        let TrackCurve::Animated { values, .. } = &clip.tracks[0].translations else {
            panic!("animated root translation");
        };
        let root = values.last().copied().expect("last root key");
        assert_eq!(root.x.to_bits(), (radius * (1.0 - cos)).to_bits());
        assert_eq!(root.z.to_bits(), (radius * sin).to_bits());
        let TrackCurve::Animated { values, .. } = &clip.tracks[0].rotations else {
            panic!("animated root rotation");
        };
        assert_eq!(values.last().copied().expect("last yaw"), det_rotation_y(angle));
        let path = arc_trajectory(&[1.0], 1.0, angle);
        assert_eq!(path[0].direction.x.to_bits(), sin.to_bits());
        assert_eq!(path[0].direction.y.to_bits(), cos.to_bits());
    }

    #[test]
    fn the_stop_clip_comes_to_rest() {
        let spec = locomotion_specs()
            .into_iter()
            .find(|s| s.name == "stop")
            .expect("stop");
        let (d1, _) = travel(&spec, 0.8);
        let (d2, _) = travel(&spec, 1.2);
        assert!((d1 - d2).abs() < 1e-6, "rests after 0.8 s");
        assert!((d1 - 1.5 * 0.4).abs() < 1e-5, "covers v T / 2");
    }
}
