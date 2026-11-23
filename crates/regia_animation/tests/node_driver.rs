// SPDX-License-Identifier: MPL-2.0
//! Procedural hinges, spins and slews on plain nodes (ADR-8410).
//!
//! The spin axle numbers are the acceptance contract derived from the DO27 rig
//! spin test. This file does not copy that source.

use std::time::Duration;

use regia_animation::clip::MorphNode;
use regia_animation::node_driver::{
    DriveKind, NodeDrive, NodeDriveParams, NodeDriveState, NodeDriver, PlainNodePlayer, PlainNodePose,
    SlewLimits, bind_times_rotation, drive_rotation,
};
use regia_animation::node_tracks::{
    NODE_TRACKS_VERSION, NodeAnimation, NodeCurve, NodeInterpolation, NodeTrack,
};
use regia_animation::prelude::AnimationPlugin;
use regia_animation::skeleton::LocalBind;
use regia_app::prelude::*;
use regia_core::math::{Mat4, Vec3};

const AXLE: Vec3 = Vec3::new(0.29, 0.135, -0.17);

fn app_at(dt: Duration) -> App {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(dt));
    app
}

fn track(index: u32, translation: Vec3, is_joint: bool, motion: Option<NodeCurve<Vec3>>) -> NodeTrack {
    NodeTrack {
        node: MorphNode {
            index,
            name: format!("n{index}"),
        },
        parent: None,
        is_joint,
        bind: LocalBind {
            translation,
            ..LocalBind::default()
        },
        matrix: None,
        translations: motion,
        rotations: None,
        scales: None,
    }
}

fn animation(nodes: Vec<NodeTrack>) -> NodeAnimation {
    NodeAnimation {
        version: NODE_TRACKS_VERSION,
        frame: Mat4::IDENTITY,
        start: 0.0,
        end: 1.0,
        nodes,
    }
}

fn slide() -> Option<NodeCurve<Vec3>> {
    Some(NodeCurve {
        times: vec![0.0, 1.0],
        values: vec![Vec3::ZERO, Vec3::X],
        interpolation: NodeInterpolation::Linear,
    })
}

fn spawn_driven(
    app: &mut App,
    animation: NodeAnimation,
    time: f32,
    drives: Vec<NodeDrive>,
    params: NodeDriveParams,
) -> Entity {
    app.world_mut()
        .spawn((
            PlainNodePlayer { animation, time },
            PlainNodePose::default(),
            NodeDriver { drives },
            params,
            NodeDriveState::default(),
        ))
        .id()
}

fn driven_entity(app: &App) -> Entity {
    let mut found = None;
    for entity in app.world().iter_entities() {
        if entity.contains::<PlainNodePose>() {
            found = Some(entity.id());
        }
    }
    found.expect("driven entity")
}

fn pose(app: &App) -> PlainNodePose {
    app.world()
        .get::<PlainNodePose>(driven_entity(app))
        .expect("plain node pose")
        .clone()
}

fn angles(app: &App) -> Vec<(f32, f32)> {
    app.world()
        .get::<NodeDriveState>(driven_entity(app))
        .expect("drive state")
        .angles
        .iter()
        .map(|a| (a.primary, a.secondary))
        .collect()
}

fn near(got: f32, want: f32, what: &str) {
    assert!((got - want).abs() < 1e-3, "{what}: {got} vs {want}");
}

#[test]
fn hinge_hits_its_limit() {
    let hinge = |min: f32, max: f32, rate: f32| DriveKind::Hinge {
        axis: Vec3::Y,
        min,
        max,
        rate,
    };
    let mut app = app_at(Duration::from_secs(1));
    spawn_driven(
        &mut app,
        animation(vec![track(1, Vec3::ZERO, false, None)]),
        0.0,
        vec![
            NodeDrive {
                node: 1,
                kind: hinge(0.0, 55.0, 1000.0),
                input: "open".into(),
            },
            NodeDrive {
                node: 1,
                kind: hinge(-180.0, 180.0, 1000.0),
                input: "second".into(),
            },
        ],
        NodeDriveParams::from_pairs([("open", 1000.0), ("open", 3.0), ("second", 90.0)]),
    );
    app.update();
    let stored = angles(&app);
    near(stored[0].0, 55.0, "huge target clamps to the limit");
    near(stored[1].0, 90.0, "second hinge");
    // Two right-multiplied 90-degree yaw turns send local +X to -X. The first hinge is only
    // 55 degrees, so this entity is not the 180-degree case. Check the limit on the pose
    // origin and use a dedicated pair below.

    let mut slow = app_at(Duration::from_millis(500));
    spawn_driven(
        &mut slow,
        animation(vec![track(1, Vec3::ZERO, false, None)]),
        0.0,
        vec![NodeDrive {
            node: 1,
            kind: hinge(0.0, 55.0, 10.0),
            input: "open".into(),
        }],
        NodeDriveParams::from_pairs([("open", 55.0)]),
    );
    slow.update();
    near(angles(&slow)[0].0, 5.0, "rate limits the approach");

    let mut outside = app_at(Duration::from_millis(500));
    let mut state = NodeDriveState::default();
    state.angles.push(regia_animation::node_driver::NodeDriveAngles {
        primary: 80.0,
        secondary: 0.0,
    });
    outside.world_mut().spawn((
        PlainNodePlayer {
            animation: animation(vec![track(1, Vec3::ZERO, false, None)]),
            time: 0.0,
        },
        PlainNodePose::default(),
        NodeDriver {
            drives: vec![NodeDrive {
                node: 1,
                kind: hinge(0.0, 55.0, 10.0),
                input: "open".into(),
            }],
        },
        NodeDriveParams::from_pairs([("open", 0.0)]),
        state,
    ));
    outside.update();
    near(
        angles(&outside)[0].0,
        75.0,
        "an angle outside the limit steps, it does not snap",
    );

    let mut nan = app_at(Duration::from_secs(1));
    let mut held = NodeDriveState::default();
    held.angles.push(regia_animation::node_driver::NodeDriveAngles {
        primary: 12.0,
        secondary: 0.0,
    });
    nan.world_mut().spawn((
        PlainNodePlayer {
            animation: animation(vec![track(1, Vec3::ZERO, false, None)]),
            time: 0.0,
        },
        PlainNodePose::default(),
        NodeDriver {
            drives: vec![NodeDrive {
                node: 1,
                kind: hinge(0.0, 55.0, 1000.0),
                input: "open".into(),
            }],
        },
        NodeDriveParams::from_pairs([("open", f32::NAN)]),
        held,
    ));
    nan.update();
    near(angles(&nan)[0].0, 12.0, "a non-finite param leaves the angle");

    let mut missing = app_at(Duration::from_secs(1));
    spawn_driven(
        &mut missing,
        animation(vec![track(1, Vec3::ZERO, false, None)]),
        0.0,
        vec![NodeDrive {
            node: 1,
            kind: hinge(-20.0, 10.0, 100.0),
            input: "open".into(),
        }],
        NodeDriveParams::default(),
    );
    missing.update();
    near(angles(&missing)[0].0, 0.0, "a missing param reads as zero");
}

#[test]
fn spin_keeps_the_axle_fixed_as_bind_times_r() {
    let kind = DriveKind::Spin { axis: Vec3::Y };
    let rotation = drive_rotation(&kind, 90.0, 0.0);
    let bind = Mat4::from_translation(AXLE);
    let driven = bind_times_rotation(bind, rotation);
    let origin = driven.transform_point3(Vec3::ZERO);
    assert!(
        origin.distance(AXLE) < 1e-4,
        "bind * R keeps the axle, origin {origin:?}"
    );
    let orbit = (Mat4::from_quat(rotation) * bind).transform_point3(Vec3::ZERO);
    assert!(
        orbit.distance(AXLE) > 1e-2,
        "R * bind moves the axle, orbit {orbit:?}"
    );
    let tip = driven.transform_point3(Vec3::X);
    near(tip.y, AXLE.y, "axle height");
    let horizontal = Vec3::new(tip.x - AXLE.x, 0.0, tip.z - AXLE.z).length();
    near(horizontal, 1.0, "unit offset");
    let expected = AXLE + Vec3::new(0.0, 0.0, -1.0);
    assert!(
        tip.distance(expected) < 1e-3,
        "right-handed yaw sends +X toward -Z, tip {tip:?}"
    );

    let mut app = app_at(Duration::from_secs(1));
    spawn_driven(
        &mut app,
        animation(vec![track(2, AXLE, false, None), track(9, AXLE, false, None)]),
        0.0,
        vec![
            NodeDrive {
                node: 2,
                kind,
                input: "speed".into(),
            },
            NodeDrive {
                node: 9,
                kind: DriveKind::Spin { axis: Vec3::ZERO },
                input: "speed".into(),
            },
        ],
        NodeDriveParams::from_pairs([("speed", 90.0)]),
    );
    app.update();
    let pose = pose(&app);
    let spun = pose.nodes.iter().find(|n| n.index == 2).expect("spun");
    let still = pose.nodes.iter().find(|n| n.index == 9).expect("zero axis");
    assert!(
        spun.clip.transform_point3(Vec3::ZERO).distance(AXLE) < 1e-4,
        "the clip pose stays the bind"
    );
    assert!(
        spun.driven.transform_point3(Vec3::ZERO).distance(AXLE) < 1e-4,
        "the applied pose keeps the axle"
    );
    assert!(
        spun.driven.transform_point3(Vec3::X).distance(expected) < 1e-3,
        "the system applies the same bind * R"
    );
    assert_eq!(still.driven, still.clip, "a zero axis adds no rotation");
    near(angles(&app)[0].0, 90.0, "one second at 90 deg/s");
}

#[test]
fn slew_never_exceeds_its_rate() {
    let wide = SlewLimits {
        yaw_min: -1000.0,
        yaw_max: 1000.0,
        pitch_min: -1000.0,
        pitch_max: 1000.0,
    };
    let kind = DriveKind::Slew {
        yaw_axis: Vec3::Y,
        pitch_axis: Vec3::NEG_X,
        max_rate: 20.0,
        limits: wide,
    };
    let mut app = app_at(Duration::from_millis(50));
    spawn_driven(
        &mut app,
        animation(vec![track(3, Vec3::ZERO, false, None)]),
        0.0,
        vec![NodeDrive {
            node: 3,
            kind,
            input: "aim".into(),
        }],
        NodeDriveParams::from_pairs([("aim", 1000.0), ("aim.pitch", 1000.0)]),
    );
    app.update();
    let (yaw, pitch) = angles(&app)[0];
    let step = 20.0 * Duration::from_millis(50).as_secs_f32();
    near(step, 1.0, "the chosen step is one degree");
    near(yaw, step, "yaw moves one step, not to the target");
    near(pitch, step, "pitch moves one step on its own axis");
    assert!(yaw.abs() <= step + 1e-4, "yaw stayed inside the rate");
    assert!(pitch.abs() <= step + 1e-4, "pitch stayed inside the rate");

    let mut near_target = app_at(Duration::from_millis(50));
    let mut started = NodeDriveState::default();
    started
        .angles
        .push(regia_animation::node_driver::NodeDriveAngles {
            primary: 1.0,
            secondary: 1.0,
        });
    near_target.world_mut().spawn((
        PlainNodePlayer {
            animation: animation(vec![track(3, Vec3::ZERO, false, None)]),
            time: 0.0,
        },
        PlainNodePose::default(),
        NodeDriver {
            drives: vec![NodeDrive {
                node: 3,
                kind: DriveKind::Slew {
                    yaw_axis: Vec3::Y,
                    pitch_axis: Vec3::NEG_X,
                    max_rate: 20.0,
                    limits: wide,
                },
                input: "aim".into(),
            }],
        },
        NodeDriveParams::from_pairs([("aim", 1.2), ("aim.pitch", 1.2)]),
        started,
    ));
    near_target.update();
    near(
        angles(&near_target)[0].0,
        1.2,
        "a nearer yaw target is not overshot",
    );
    near(
        angles(&near_target)[0].1,
        1.2,
        "a nearer pitch target is not overshot",
    );

    let mut outside = app_at(Duration::from_millis(50));
    let mut high = NodeDriveState::default();
    high.angles.push(regia_animation::node_driver::NodeDriveAngles {
        primary: 100.0,
        secondary: -100.0,
    });
    outside.world_mut().spawn((
        PlainNodePlayer {
            animation: animation(vec![track(3, Vec3::ZERO, false, None)]),
            time: 0.0,
        },
        PlainNodePose::default(),
        NodeDriver {
            drives: vec![NodeDrive {
                node: 3,
                kind: DriveKind::Slew {
                    yaw_axis: Vec3::Y,
                    pitch_axis: Vec3::NEG_X,
                    max_rate: 20.0,
                    limits: SlewLimits {
                        yaw_min: -10.0,
                        yaw_max: 10.0,
                        pitch_min: -10.0,
                        pitch_max: 10.0,
                    },
                },
                input: "aim".into(),
            }],
        },
        NodeDriveParams::from_pairs([("aim", 0.0)]),
        high,
    ));
    outside.update();
    near(angles(&outside)[0].0, 99.0, "yaw outside its limit steps inward");
    near(angles(&outside)[0].1, -99.0, "a missing pitch param aims at zero");

    let mut frozen = app_at(Duration::from_secs(1));
    spawn_driven(
        &mut frozen,
        animation(vec![track(3, Vec3::ZERO, false, None)]),
        0.0,
        vec![NodeDrive {
            node: 3,
            kind: DriveKind::Slew {
                yaw_axis: Vec3::Y,
                pitch_axis: Vec3::X,
                max_rate: 0.0,
                limits: wide,
            },
            input: "aim".into(),
        }],
        NodeDriveParams::from_pairs([("aim", 40.0), ("aim.pitch", 40.0)]),
    );
    frozen.update();
    near(angles(&frozen)[0].0, 0.0, "a zero rate does not move yaw");
    near(angles(&frozen)[0].1, 0.0, "a zero rate does not move pitch");
}

#[test]
fn the_driver_applies_after_the_clip_pose() {
    let mut app = app_at(Duration::from_secs(1));
    spawn_driven(
        &mut app,
        animation(vec![
            track(4, Vec3::ZERO, false, slide()),
            track(5, Vec3::Y, true, None),
        ]),
        0.0,
        vec![
            NodeDrive {
                node: 4,
                kind: DriveKind::Spin { axis: Vec3::Y },
                input: "speed".into(),
            },
            NodeDrive {
                node: 5,
                kind: DriveKind::Spin { axis: Vec3::Y },
                input: "speed".into(),
            },
        ],
        NodeDriveParams::from_pairs([("speed", 90.0)]),
    );
    app.update();
    let first = pose(&app);
    let part = first.nodes.iter().find(|n| n.index == 4).expect("part");
    let joint = first.nodes.iter().find(|n| n.index == 5).expect("joint");
    assert_eq!(part.clip.transform_point3(Vec3::ZERO), Vec3::ZERO);
    let tip = part.driven.transform_point3(Vec3::X);
    assert!(
        tip.distance(Vec3::new(0.0, 0.0, -1.0)) < 1e-3,
        "driven is the clip pose times R, tip {tip:?}"
    );
    assert_eq!(joint.driven, joint.clip, "a joint keeps the clip pose");
    near(
        angles(&app)[1].0,
        90.0,
        "the joint drive still advances its angle",
    );

    let entity = driven_entity(&app);
    app.world_mut()
        .get_mut::<PlainNodePlayer>(entity)
        .expect("player")
        .time = 1.0;
    app.world_mut()
        .get_mut::<NodeDriveParams>(entity)
        .expect("params")
        .set("speed", 0.0);
    app.update();
    let second = pose(&app);
    let part = second.nodes.iter().find(|n| n.index == 4).expect("part");
    assert!(
        part.clip.transform_point3(Vec3::ZERO).distance(Vec3::X) < 1e-4,
        "the later clip sample moved the node"
    );
    let tip = part.driven.transform_point3(Vec3::X);
    assert!(
        tip.distance(Vec3::new(1.0, 0.0, -1.0)) < 1e-3,
        "the stored angle multiplies the new clip pose, tip {tip:?}"
    );
    assert_ne!(
        part.driven, part.clip,
        "sampling after the drive would have wiped the rotation"
    );
    near(angles(&app)[0].0, 90.0, "zero speed keeps the accumulated angle");
}
