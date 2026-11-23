// SPDX-License-Identifier: MPL-2.0
//! Planted-foot gait content for motion matching (ADR-2858): the three-joint rig of
//! [`super::procedural`], with clips whose feet stand still in the world while in stance and
//! swing (lifting) to the next footfall. Walks at three speeds, a left and a right turn, a left
//! and a right strafe (moving sideways while facing forward) and an idle; the walks can be
//! mirrored (the right foot leads), which gives the mirrored-gait fixture of the phase tests.
//!
//! Each foot touches down once per `period`; it is planted for `duty · period` seconds at the
//! point under its hip at mid-stance, then swings along a smoothstep to its next footfall,
//! lifting `0.08 m` at mid-swing. The left foot's cycle phase is `t / period + lead` (it touches
//! down at phase 0), the right foot's half a cycle later. The source enables contact labels
//! and the phase group (both at weight 0, the default), so the same database serves the foot-lock
//! output, the contact and phase tests and the strafe tests.

// Source: original design (a stance/swing foot model with a duty factor, a foot planted at
// mid-stance under the hip and a lifted smoothstep swing), test content only. Original code;
// nothing ported.

use std::f32::consts::PI;

use regia_core::math::{Quat, Vec2, Vec3};

use super::asset::{EntrySource, MotionDatabaseSource, SchemaSource};
use super::procedural::{ProceduralLocomotion, locomotion_skeleton};
use super::{ContactSettings, TrajectoryPoint};
use crate::clip::{AnimationClip, JointTrack, TrackCurve};
use crate::num::{det_rotation_y, det_sin_cos};

/// The skeleton's asset path in [`gait_set`]'s source.
pub const GAIT_SKELETON_PATH: &str = "procedural/gait#skeleton/root";

/// Key rate of the gait clips, keys per second.
const KEY_RATE: f32 = 60.0;
/// Swing lift at mid-swing, meters.
const LIFT: f32 = 0.08;
/// Half the distance between the feet, meters (the rig's hip width).
const HALF_WIDTH: f32 = 0.1;

/// One gait clip.
#[derive(Clone, Debug, PartialEq)]
pub struct GaitSpec {
    /// Clip name (also the last part of its asset path).
    pub name: &'static str,
    /// Velocity in the character frame (`x` right, `y` forward), meters per second.
    pub velocity: Vec2,
    /// Turn rate, radians per second (positive toward +X).
    pub turn: f32,
    /// Step period of one foot, seconds.
    pub period: f32,
    /// Fraction of the period a foot is planted.
    pub duty: f32,
    /// The left foot's phase at `t = 0` (the right foot's is half a cycle later).
    pub lead: f32,
    /// Clip length, seconds (whole periods for a looping clip).
    pub duration: f32,
    /// Tag names of every frame.
    pub tags: &'static [&'static str],
    /// Add the entry mirrored too (left and right swapped).
    pub mirrored: bool,
}

impl GaitSpec {
    /// The clip's asset path in [`gait_set`]'s source.
    pub fn path(&self) -> String {
        format!("procedural/gait#clip/{}", self.name)
    }

    fn moves(&self) -> bool {
        self.velocity.length_squared() > 1e-8 || self.turn.abs() > 1e-6
    }

    /// Ground position and heading at `t`.
    fn root(&self, t: f32) -> (Vec3, f32) {
        let (vx, vz) = (self.velocity.x, self.velocity.y);
        let w = self.turn;
        let heading = w * t;
        let position = if w.abs() < 1e-6 {
            Vec3::new(vx * t, 0.0, vz * t)
        } else {
            let (s, c) = det_sin_cos(heading);
            Vec3::new(
                vx * s / w + vz * (1.0 - c) / w,
                0.0,
                -vx * (1.0 - c) / w + vz * s / w,
            )
        };
        (position, heading)
    }

    /// World position of foot `side` (−1 left, +1 right) at `t`.
    fn foot(&self, side: f32, t: f32) -> Vec3 {
        let offset = if side < 0.0 { self.lead } else { self.lead + 0.5 };
        let cycles = t / self.period + offset;
        let k = cycles.floor();
        let phase = cycles - k;
        let plant = |k: f32| {
            let touch = (k - offset) * self.period;
            let (p, h) = self.root(touch + 0.5 * self.duty * self.period);
            p + det_rotation_y(h) * Vec3::new(side * HALF_WIDTH, 0.0, 0.0)
        };
        if phase < self.duty {
            plant(k)
        } else {
            let u = (phase - self.duty) / (1.0 - self.duty);
            let smooth = u * u * (3.0 - 2.0 * u);
            plant(k).lerp(plant(k + 1.0), smooth) + Vec3::Y * (LIFT * det_sin_cos(PI * u).0)
        }
    }
}

/// The clip of `spec`: root translation and yaw keys, foot keys in the root's frame.
pub fn gait_clip(spec: &GaitSpec) -> AnimationClip {
    let keys = (spec.duration * KEY_RATE).round() as usize;
    let times: Vec<f32> = (0..=keys)
        .map(|k| (k as f32 / KEY_RATE).min(spec.duration))
        .collect();
    let mut clip = AnimationClip::empty(spec.name, spec.duration);
    clip.sample_rate = KEY_RATE;
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: times.clone(),
            values: times.iter().map(|&t| spec.root(t).0).collect(),
            exact_rotation: false,
        },
        rotations: TrackCurve::Animated {
            times: times.clone(),
            values: times.iter().map(|&t| det_rotation_y(spec.root(t).1)).collect(),
            exact_rotation: true,
        },
        scales: TrackCurve::Static(Vec3::ONE),
    });
    for (joint, side) in [(1u32, -1.0f32), (2u32, 1.0f32)] {
        let values = times
            .iter()
            .map(|&t| {
                if !spec.moves() {
                    return Vec3::new(side * HALF_WIDTH, 0.0, 0.0);
                }
                let (p, h) = spec.root(t);
                det_rotation_y(-h) * (spec.foot(side, t) - p)
            })
            .collect();
        clip.tracks.push(JointTrack {
            joint,
            translations: TrackCurve::Animated {
                times: times.clone(),
                values,
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
    }
    clip
}

/// The gait clips, in database entry order: idle, three walks (mirrored), a left and a right
/// turn, a left and a right strafe.
pub fn gait_specs() -> Vec<GaitSpec> {
    let walk = |name, speed: f32, period: f32| GaitSpec {
        name,
        velocity: Vec2::new(0.0, speed),
        turn: 0.0,
        period,
        duty: 0.6,
        lead: 0.0,
        duration: 2.0 * period,
        tags: &["walk"],
        mirrored: true,
    };
    let other = |name, velocity: Vec2, turn: f32, tags| GaitSpec {
        name,
        velocity,
        turn,
        period: 1.0,
        duty: 0.6,
        lead: 0.0,
        duration: 2.0,
        tags,
        mirrored: false,
    };
    vec![
        GaitSpec {
            mirrored: false,
            ..other("idle", Vec2::ZERO, 0.0, &["idle"])
        },
        walk("walk_slow", 1.0, 1.1),
        walk("walk", 1.5, 1.0),
        walk("walk_fast", 2.0, 0.9),
        other("turn_left", Vec2::new(0.0, 1.5), -1.0, &["walk", "turn"]),
        other("turn_right", Vec2::new(0.0, 1.5), 1.0, &["walk", "turn"]),
        other("strafe_left", Vec2::new(-1.2, 0.0), 0.0, &["strafe"]),
        other("strafe_right", Vec2::new(1.2, 0.0), 0.0, &["strafe"]),
    ]
}

/// The gait set: rig, clips and a source with contact labels and the phase group (weights 0),
/// mirror pairs for the walks, tags `idle`, `walk`, `turn`, `strafe`.
pub fn gait_set() -> ProceduralLocomotion {
    let specs = gait_specs();
    let clips = specs.iter().map(|s| (s.path(), gait_clip(s))).collect();
    let source = MotionDatabaseSource {
        format_version: super::asset::MOTIONDB_FORMAT_VERSION,
        skeleton: GAIT_SKELETON_PATH.into(),
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
            contacts: Some(ContactSettings::default()),
        },
        tags: ["idle", "walk", "turn", "strafe"]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        mirror_pairs: vec![("foot_l".into(), "foot_r".into())],
        entries: specs
            .iter()
            .map(|s| EntrySource {
                clip: s.path(),
                looping: true,
                tags: s.tags.iter().map(|t| (*t).to_string()).collect(),
                mirrored: s.mirrored,
                trim: (0.0, 0.0),
                spans: Vec::new(),
                distance_matched: false,
            })
            .collect(),
    };
    ProceduralLocomotion {
        skeleton: locomotion_skeleton(),
        clips,
        source,
    }
}

/// A straight desired path at `velocity` (character frame) with constant facing: a strafe when
/// `velocity.x` is not zero.
pub fn strafe_trajectory(times: &[f32], velocity: Vec2) -> Vec<TrajectoryPoint> {
    times
        .iter()
        .map(|&t| TrajectoryPoint {
            position: velocity * t,
            direction: Vec2::Y,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_planted_foot_stands_still_in_the_world_and_the_swing_lifts() {
        let spec = gait_specs().into_iter().find(|s| s.name == "walk").expect("walk");
        // Left foot: stance for phase [0, 0.6).
        let a = spec.foot(-1.0, 0.1);
        let b = spec.foot(-1.0, 0.5);
        assert!(a.distance(b) < 1e-5, "{a} {b}");
        assert_eq!(a.y, 0.0);
        let swing = spec.foot(-1.0, 0.8);
        assert!(swing.y > 0.05, "{swing}");
        // The next footfall is one stride (speed · period) ahead.
        let next = spec.foot(-1.0, 1.1);
        assert!((next.z - a.z - 1.5).abs() < 1e-4, "{a} {next}");
    }

    /// ADR-4411 / audit animation1-determinism L9: gait roots and yaw keys use portable trig.
    #[test]
    fn gait_turn_keys_use_the_portable_trig_oracle() {
        let angle = f32::from_bits(0x3edf_285b);
        let spec = GaitSpec {
            name: "portable_turn",
            velocity: Vec2::new(0.0, 1.0),
            turn: angle,
            period: 1.0,
            duty: 0.6,
            lead: 0.0,
            duration: 1.0,
            tags: &[],
            mirrored: false,
        };
        let (sin, cos) = det_sin_cos(angle);
        let (root, heading) = spec.root(1.0);
        assert_eq!(heading.to_bits(), angle.to_bits());
        assert_eq!(root.x.to_bits(), ((1.0 - cos) / angle).to_bits());
        assert_eq!(root.z.to_bits(), (sin / angle).to_bits());
        let clip = gait_clip(&spec);
        let TrackCurve::Animated { values, .. } = &clip.tracks[0].rotations else {
            panic!("animated root rotation");
        };
        assert_eq!(values.last().copied().expect("last yaw"), det_rotation_y(angle));
    }

    #[test]
    fn the_set_builds_with_contacts_and_phases() {
        let set = gait_set();
        let db = set.build().expect("database");
        // Three mirrored walks add three ranges.
        assert_eq!(db.clips().len(), 11);
        let walk = &db.clips()[3];
        assert_eq!(walk.name, "walk");
        let planted = (walk.start..walk.start + walk.frames)
            .filter(|&f| db.contacts(f).expect("frame")[0])
            .count();
        // Duty 0.6 with hysteresis: about 60 % of the frames.
        assert!((30..=42).contains(&planted), "{planted} of {}", walk.frames);
        // Every walk frame has a defined phase; the idle has none (no touchdown).
        assert!(db.phases(walk.start).expect("frame").iter().all(Option::is_some));
        assert!(
            db.phases(db.clips()[0].start)
                .expect("frame")
                .iter()
                .all(Option::is_none)
        );
        // The mirrored walk is the walk with the other foot leading: its left phase is the
        // walk's right phase.
        let mirrored = &db.clips()[4];
        assert!(mirrored.mirrored);
        let p = db.phases(walk.start + 5).expect("f");
        let m = db.phases(mirrored.start + 5).expect("f");
        let d = super::super::contacts::phase_distance(p[1].expect("r"), m[0].expect("l"));
        assert!(d < 0.05, "{p:?} vs {m:?}");
    }
}
