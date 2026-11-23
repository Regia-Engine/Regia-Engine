// SPDX-License-Identifier: MPL-2.0
//! A built-in procedural biped (skeleton + clips) for the animation editor's preview when no
//! character is loaded, and for the editor's own tests and screenshots (the sample Fox is
//! fetched at first run, so tests cannot rely on it).
//!
//! Clip names match the `third_person` sample's graph (`Survey`, `Walk`, `Run`) plus `Idle`,
//! `Wave` and a one-shot `Jump`, so its locomotion graph previews out of the box. Motion is
//! sinusoidal joint rotation (a standard procedural gait: legs swing in antiphase, arms
//! counter-swing); walk and run carry XZ root motion and footstep notifies.

use std::sync::Arc;

use regia_animation::clip::{AnimationClip, JointTrack, Notify, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::skeleton::{LocalBind, Skeleton};
use regia_core::math::{Quat, Vec3};

/// Joint names of the demo biped, parents first.
pub const JOINTS: [(&str, Option<usize>, [f32; 3]); 17] = [
    ("root", None, [0.0, 0.0, 0.0]),
    ("pelvis", Some(0), [0.0, 0.95, 0.0]),
    ("spine", Some(1), [0.0, 0.2, 0.0]),
    ("chest", Some(2), [0.0, 0.25, 0.0]),
    ("neck", Some(3), [0.0, 0.22, 0.0]),
    ("head", Some(4), [0.0, 0.12, 0.0]),
    ("upperarm_l", Some(3), [0.2, 0.15, 0.0]),
    ("forearm_l", Some(6), [0.0, -0.28, 0.0]),
    ("hand_l", Some(7), [0.0, -0.25, 0.0]),
    ("upperarm_r", Some(3), [-0.2, 0.15, 0.0]),
    ("forearm_r", Some(9), [0.0, -0.28, 0.0]),
    ("hand_r", Some(10), [0.0, -0.25, 0.0]),
    ("thigh_l", Some(1), [0.1, -0.05, 0.0]),
    ("shin_l", Some(12), [0.0, -0.42, 0.0]),
    ("foot_l", Some(13), [0.0, -0.42, 0.05]),
    ("thigh_r", Some(1), [-0.1, -0.05, 0.0]),
    ("shin_r", Some(15), [0.0, -0.42, 0.0]),
];

/// The demo skeleton (16 joints + right foot).
#[must_use]
pub fn skeleton() -> Arc<Skeleton> {
    let mut names: Vec<String> = JOINTS.iter().map(|j| j.0.to_string()).collect();
    let mut parents: Vec<Option<u32>> = JOINTS.iter().map(|j| j.1.map(|p| p as u32)).collect();
    let mut binds: Vec<LocalBind> = JOINTS
        .iter()
        .map(|j| LocalBind {
            translation: Vec3::from_array(j.2),
            ..LocalBind::default()
        })
        .collect();
    names.push("foot_r".into());
    parents.push(Some(16));
    binds.push(LocalBind {
        translation: Vec3::new(0.0, -0.42, 0.05),
        ..LocalBind::default()
    });
    Skeleton::from_local_bind(names, parents, binds)
        .map(Arc::new)
        .expect("the demo joint table is a valid, topologically sorted hierarchy")
}

fn joint(name: &str) -> u32 {
    if name == "foot_r" {
        return JOINTS.len() as u32;
    }
    JOINTS.iter().position(|j| j.0 == name).unwrap_or(0) as u32
}

/// A rotation track about `axis` of `amplitude * sin(2π (t / period) + phase)`.
fn swing(name: &str, axis: Vec3, amplitude: f32, phase: f32, duration: f32, rate: f32) -> JointTrack {
    let steps = (duration * rate).round().max(2.0) as usize;
    let times: Vec<f32> = (0..=steps).map(|i| i as f32 / rate).collect();
    let rotations = times
        .iter()
        .map(|t| {
            let angle = amplitude * (std::f32::consts::TAU * t / duration + phase).sin();
            Quat::from_axis_angle(axis, angle)
        })
        .collect();
    let bind = JOINTS
        .iter()
        .find(|j| j.0 == name)
        .map_or(Vec3::new(0.0, -0.42, 0.05), |j| Vec3::from_array(j.2));
    JointTrack {
        joint: joint(name),
        translations: TrackCurve::Static(bind),
        rotations: TrackCurve::Animated {
            times,
            values: rotations,
            exact_rotation: false,
        },
        scales: TrackCurve::Static(Vec3::ONE),
    }
}

fn gait(name: &str, duration: f32, stride: f32, speed: f32) -> AnimationClip {
    let rate = 30.0;
    let mut clip = AnimationClip::empty(name, duration);
    clip.sample_rate = rate;
    let x = Vec3::X;
    for (joint, amp, phase) in [
        ("thigh_l", stride, 0.0),
        ("thigh_r", stride, std::f32::consts::PI),
        ("shin_l", stride * 0.6, -1.2),
        ("shin_r", stride * 0.6, std::f32::consts::PI - 1.2),
        ("upperarm_l", stride * 0.7, std::f32::consts::PI),
        ("upperarm_r", stride * 0.7, 0.0),
    ] {
        clip.tracks.push(swing(joint, x, amp, phase, duration, rate));
    }
    // Root: moves forward at `speed` m/s (root motion), pelvis bobs.
    let times: Vec<f32> = (0..=((duration * rate) as usize))
        .map(|i| i as f32 / rate)
        .collect();
    clip.tracks.push(JointTrack {
        joint: joint("root"),
        translations: TrackCurve::Animated {
            values: times.iter().map(|t| Vec3::new(0.0, 0.0, speed * t)).collect(),
            times: times.clone(),
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip.tracks.push(JointTrack {
        joint: joint("pelvis"),
        translations: TrackCurve::Animated {
            values: times
                .iter()
                .map(|t| {
                    Vec3::new(
                        0.0,
                        0.95 + 0.03 * (std::f32::consts::TAU * 2.0 * t / duration).cos(),
                        0.0,
                    )
                })
                .collect(),
            times,
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip.tracks.sort_by_key(|t| t.joint);
    clip.root_motion = Some(RootMotionTrack {
        joint: 0,
        mode: RootMotionMode::XZ,
    });
    clip.notifies = vec![
        Notify {
            time: duration * 0.25,
            name: "Footstep_L".into(),
            payload: None,
        },
        Notify {
            time: duration * 0.75,
            name: "Footstep_R".into(),
            payload: None,
        },
    ];
    clip
}

fn idle(name: &str, head_turn: f32) -> AnimationClip {
    let duration = 2.4;
    let rate = 30.0;
    let mut clip = AnimationClip::empty(name, duration);
    clip.sample_rate = rate;
    clip.tracks
        .push(swing("chest", Vec3::X, 0.04, 0.0, duration, rate));
    clip.tracks
        .push(swing("head", Vec3::Y, head_turn, 0.0, duration, rate));
    clip.tracks.sort_by_key(|t| t.joint);
    clip
}

fn wave() -> AnimationClip {
    let duration = 1.2;
    let rate = 30.0;
    let mut clip = AnimationClip::empty("Wave", duration);
    clip.sample_rate = rate;
    let mut raise = swing("upperarm_r", Vec3::Z, 0.0, 0.0, duration, rate);
    raise.rotations = TrackCurve::Static(Quat::from_rotation_z(-2.6));
    clip.tracks.push(raise);
    clip.tracks
        .push(swing("forearm_r", Vec3::Z, 0.5, 0.0, duration, rate));
    clip.tracks.sort_by_key(|t| t.joint);
    clip
}

fn jump() -> AnimationClip {
    let duration = 0.8;
    let mut clip = gait("Jump", duration, 0.2, 0.0);
    clip.root_motion = None;
    clip.notifies = vec![Notify {
        time: 0.7,
        name: "Land".into(),
        payload: None,
    }];
    clip
}

/// The demo clips: Idle, Survey, Walk, Run, Wave, Jump.
#[must_use]
pub fn clips() -> Vec<Arc<AnimationClip>> {
    vec![
        Arc::new(idle("Idle", 0.05)),
        Arc::new(idle("Survey", 0.6)),
        Arc::new(gait("Walk", 1.0, 0.45, 1.4)),
        Arc::new(gait("Run", 0.6, 0.8, 4.5)),
        Arc::new(wave()),
        Arc::new(jump()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_rig_is_consistent() {
        let skeleton = skeleton();
        assert_eq!(skeleton.len(), 18);
        assert_eq!(skeleton.joint_index("foot_r"), Some(17));
        for clip in clips() {
            for track in &clip.tracks {
                assert!((track.joint as usize) < skeleton.len(), "{}", clip.name);
            }
            assert!(
                clip.tracks.windows(2).all(|w| w[0].joint < w[1].joint),
                "{}",
                clip.name
            );
        }
        let walk = clips().into_iter().find(|c| c.name == "Walk").expect("walk");
        let (delta, _) = walk.root_motion_delta(0.0, 0.5, false).expect("root motion");
        assert!((delta.z - 0.7).abs() < 1e-3, "{delta:?}");
    }
}
