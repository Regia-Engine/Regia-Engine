// SPDX-License-Identifier: MPL-2.0
//! A playing warp walks the rest of its window once, not once per frame.
//!
//! Thirty fixed steps inside one window must call the rest-of-window root-motion walk once.
//! The rig still lands on the warp target.

use std::sync::Arc;

use regia_animation::clip::{AnimationClip, JointTrack, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::hot_count::{self, root_motion_rest_walks};
use regia_animation::montage::{Montage, WarpWindow};
use regia_animation::montage_player::{MontageFrame, MontagePlayer, PlayOptions, RigPlacement, SlotDef};
use regia_animation::pose::{Pose, translated};
use regia_animation::{ClipLibrary, Skeleton, WarpTarget};
use regia_core::math::{Quat, Vec3};

fn skeleton() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["root".into(), "arm".into()],
        vec![None, Some(0)],
        vec![translated(Vec3::ZERO), translated(Vec3::Y)],
    )
    .expect("skeleton")
}

/// A 2 s clip: the root walks 1 m/s toward −Z.
fn strike() -> AnimationClip {
    let duration = 2.0;
    let mut clip = AnimationClip::empty("strike", duration);
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: vec![0.0, duration],
            values: vec![Vec3::ZERO, Vec3::new(0.0, 0.0, -duration)],
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip.root_motion = Some(RootMotionTrack {
        joint: 0,
        mode: RootMotionMode::XZ,
    });
    clip
}

#[test]
fn warp_rest_motion_is_not_resampled_every_step() {
    let _guard = hot_count::lock_for_test();
    let skeleton = skeleton();
    let mut clips = ClipLibrary::new();
    clips.insert(strike());
    let mut player = MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], clips);
    let mut montage = Montage::single("lunge", "full_body", "strike");
    montage.blend_in.time = 0.0;
    montage.warps = vec![WarpWindow {
        target: "enemy".into(),
        start: 0.0,
        end: 1.0,
        translation: true,
        vertical: false,
        rotation: false,
        ..WarpWindow::new("", 0.0, 0.0)
    }];
    player.play(Arc::new(montage), PlayOptions::default());
    player.set_warp_target("enemy", WarpTarget::at(Vec3::new(0.0, 0.0, -2.0)));

    let dt = 1.0 / 30.0;
    let mut placement = RigPlacement::default();
    let mut pose = Pose::from_identity(skeleton.len());
    let before = root_motion_rest_walks();
    for _ in 0..30 {
        let mut root = (Vec3::ZERO, Quat::IDENTITY);
        let frame = MontageFrame {
            skeleton: &skeleton,
            graph_clips: None,
            delta: dt,
            rig: placement,
        };
        player.evaluate(&frame, &mut pose, &mut root, &mut Vec::new());
        placement.position += placement.rotation * root.0;
        placement.rotation = (placement.rotation * root.1).normalize();
    }
    let walks = root_motion_rest_walks() - before;
    assert_eq!(
        walks, 1,
        "thirty steps inside one warp window walked the rest {walks} times"
    );
    let miss = (placement.position - Vec3::new(0.0, 0.0, -2.0)).length();
    assert!(
        miss < 0.02,
        "the warp still lands on the target (miss {miss}, at {:?})",
        placement.position
    );
}
