// SPDX-License-Identifier: MPL-2.0
//! Heel and toe foot placement (animation-09): stairs, ramps and toe bend on top of the foot
//! planter (`foot_ik::shape`, `FootPlanter::plan_with_soles`).
//!
//! These tests fail to compile on the foot-planting base, which has no heel or toe input. The
//! single-probe planter (`FootPlanter::plan`) stays bit-identical: its own test file
//! (`foot_plant.rs`) runs unchanged, and `single_probe_input_is_bit_identical_to_f` compares the
//! two entry points bit for bit.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_animation::demo_rig::{
    DEMO_LEGS, DemoGait, DemoRig, demo_humanoid, demo_in_place, demo_locomotion,
};
use regia_animation::foot_ik::{
    FootGround, FootIkLeg, FootPlant, FootPlantInput, FootPlantLeg, FootPlantSettings, FootPlanter,
    FootShape, FootShapeSettings, FootSole, apply_foot_plant_with_toes, foot_shape_from_bind,
    probe_foot_sole,
};
use regia_animation::ik::{IkChain, joint_position};
use regia_animation::{Pose, Skeleton};
use regia_core::math::{Mat4, Quat, Vec3};

const DT: f32 = 1.0 / 60.0;
const RISE: f32 = 0.18;
const RUN: f32 = 0.28;
const STEPS: f32 = 10.0;
/// Character speed over the authored walk speed (the clip skates, as in `foot_plant.rs`).
const MISMATCH: f32 = 1.15;

/// Ten steps up along world −Z from z = 0: risers at s = 0, 0.28, … 2.52 (s = −z), then a landing.
fn stairs(p: Vec3) -> f32 {
    let s = -p.z;
    if s < 0.0 {
        0.0
    } else {
        RISE * ((s / RUN).floor() + 1.0).min(STEPS)
    }
}

/// The heel, ball and toe of the demo rig's left foot (the plain rig: identity bind frames).
fn demo_shape(skeleton: &Skeleton) -> FootShape {
    foot_shape_from_bind(skeleton, 14, Some(15), &FootShapeSettings::default()).expect("shape")
}

fn sole_on(ground: impl Fn(Vec3) -> f32, foot: Mat4, anchor: Vec3, shape: &FootShape) -> FootSole {
    let s = FootPlantSettings::default();
    probe_foot_sole(
        foot,
        anchor,
        shape,
        s.step_edge,
        FootShapeSettings::default().edge_steps,
        |p| Some(Vec3::new(p.x, ground(p), p.z)),
    )
}

/// Where a placed sole point sits over the stairs: its height over the ground under it.
fn gap(p: Vec3) -> f32 {
    p.y - stairs(p)
}

/// Fraction of the heel-to-toe segment whose ground is within 1 cm under the sole height `y`
/// (the part of a flat foot that rests on a tread).
fn supported(heel: Vec3, toe: Vec3, y: f32) -> f32 {
    let n = 200;
    let on = (0..=n)
        .filter(|k| {
            let p = heel.lerp(toe, *k as f32 / n as f32);
            (y - stairs(p)).abs() <= 0.01
        })
        .count();
    on as f32 / (n + 1) as f32
}

/// One placed foot, classified the way the acceptance reads it.
#[derive(Debug, Default)]
struct Placed {
    heel_gap: f32,
    toe_gap: f32,
    /// Both heel and toe within 1 cm over the ground under them.
    flush: bool,
    /// Flat on one tread with at least `min_support` of the foot on it (the rest hangs past the
    /// nosing at the tread's height).
    resting: bool,
}

fn classify(heel: Vec3, toe: Vec3, min_support: f32) -> Placed {
    let (heel_gap, toe_gap) = (gap(heel), gap(toe));
    let flush = heel_gap <= 0.01 && toe_gap <= 0.01;
    let level = (heel.y - toe.y).abs() <= 0.005;
    let resting = level
        && heel_gap.min(toe_gap) <= 0.01
        && supported(heel, toe, heel.y.min(toe.y)) >= min_support - 0.02;
    Placed {
        heel_gap,
        toe_gap,
        flush,
        resting,
    }
}

/// One leg standing still for `frames` frames with its ankle over stair point `s` (facing up the
/// stairs for `yaw` 0, down for π), planned with or without heel and toe samples. Returns the
/// last plan, the rig transform and the clip ankle.
fn stand(s: f32, yaw: f32, soles: bool, frames: usize) -> (FootPlant, Mat4, Vec3) {
    let skeleton = demo_humanoid(&DemoRig::default()).expect("rig");
    let shape = demo_shape(&skeleton);
    let rotation = Quat::from_rotation_y(yaw);
    let rig = Vec3::new(0.0, 0.27, -s) - rotation * Vec3::new(-0.1, 0.0, 0.0);
    let to_world = Mat4::from_rotation_translation(rotation, rig);
    let ankle_object = Vec3::new(-0.1, 0.08, 0.0);
    let foot = to_world * Mat4::from_translation(ankle_object);
    let ankle = foot.w_axis.truncate();
    let mut planter = FootPlanter::new(FootPlantSettings::default());
    let mut plan = FootPlant::default();
    for _ in 0..frames {
        let probe = planter.pin(0).unwrap_or(ankle);
        let leg = FootPlantLeg {
            ground: FootGround::Contact(Vec3::new(probe.x, stairs(probe), probe.z)),
            ..FootPlantLeg::new(
                to_world.transform_point3(Vec3::new(-0.1, 0.98, 0.0)),
                ankle,
                0.9,
                0.08,
            )
        };
        let input = FootPlantInput {
            rig_translation: rig,
            rig_rotation: rotation,
            legs: std::slice::from_ref(&leg),
            active: true,
            dt: DT,
        };
        plan = if soles {
            let sole = sole_on(stairs, foot, probe, &shape);
            planter.plan_with_soles(&input, &[sole])
        } else {
            planter.plan(&input)
        };
    }
    (plan, to_world, ankle)
}

/// Acceptance: a sweep of foot positions across a 0.18 m riser, both facing up and down the
/// stairs. Each placed heel and toe is within 1 cm over its ground, or the foot rests flat on
/// one tread with at least `min_support` of it there (moved onto that tread when it mostly
/// covered the other one, never farther than `max_edge_shift`); no point is ever below the
/// ground. Without heel and toe samples (the single ankle probe) the same sweep sinks toes into
/// the riser.
#[test]
fn foot_on_a_stair_edge_never_floats_or_sinks() {
    let settings = FootPlantSettings::default();
    let skeleton = demo_humanoid(&DemoRig::default()).expect("rig");
    let shape = demo_shape(&skeleton);
    let (mut flush, mut resting, mut shifted) = (0, 0, 0);
    let mut worst_single_sink: f32 = 0.0;
    for yaw in [0.0_f32, std::f32::consts::PI] {
        for k in -25..=25 {
            // Ankle within 25 cm of the riser between treads 1 and 2.
            let s = RUN + k as f32 * 0.01;
            let (plan, to_world, ankle) = stand(s, yaw, true, 90);
            let leg = &plan.legs[0];
            let placed = to_world.transform_point3(leg.target);
            let rotation = to_world.to_scale_rotation_translation().1;
            let heel = placed + rotation * (leg.tilt * shape.heel);
            let toe = placed + rotation * (leg.tilt * shape.toe);
            let p = classify(heel, toe, settings.min_support);
            assert!(
                p.heel_gap >= -1.0e-4 && p.toe_gap >= -1.0e-4,
                "yaw {yaw} s {s}: below the ground {p:?}"
            );
            assert!(
                p.flush || p.resting,
                "yaw {yaw} s {s}: neither flush nor resting on a tread {p:?}"
            );
            let moved = Vec3::new(placed.x - ankle.x, 0.0, placed.z - ankle.z).length();
            assert!(
                moved <= settings.max_edge_shift + 1.0e-4,
                "yaw {yaw} s {s}: moved {moved}"
            );
            if moved > 1.0e-3 {
                shifted += 1;
            }
            if p.flush {
                flush += 1;
            } else {
                resting += 1;
            }
            // The report reads the same way: planted, no gap over 1 cm, nothing under the ground.
            for g in [leg.heel_gap, leg.toe_gap] {
                assert!(
                    (-1.0e-3..=0.011).contains(&g),
                    "yaw {yaw} s {s}: report {leg:?} {p:?}"
                );
            }
            // The single ankle probe for comparison.
            let (single, to_world, _) = stand(s, yaw, false, 90);
            let leg = &single.legs[0];
            let placed = to_world.transform_point3(leg.target);
            let toe = placed + rotation * (leg.tilt * shape.toe);
            let heel = placed + rotation * (leg.tilt * shape.heel);
            worst_single_sink = worst_single_sink.max(-gap(toe)).max(-gap(heel));
        }
    }
    println!(
        "foot_stairs sweep: flush={flush} resting={resting} shifted={shifted} single_probe_worst_sink={worst_single_sink:.4}m"
    );
    assert!(
        flush > 0 && resting > 0 && shifted > 0,
        "every branch of the stair rule ran"
    );
    assert!(
        worst_single_sink > 0.05,
        "the scenario discriminates: the single probe sinks a toe or heel {worst_single_sink} m"
    );
}

/// Per walk: penetration and float of planted soles on the stairs.
#[derive(Debug, Default)]
struct StairWalk {
    planted_frames: usize,
    /// Deepest a planted heel or toe went under the ground, meters.
    penetration: f32,
    /// Largest height of a planted foot's lowest sole point over the ground, meters.
    float: f32,
    /// Planted frames that were neither flush nor resting on a tread.
    unsupported: usize,
    /// Frames whose planted foot stood on a tread above the first one.
    on_stairs: usize,
    /// Largest distance of a solved planted ankle from the planner's target, meters (the leg
    /// could not reach it).
    reach_gap: f32,
}

/// The demo walk over ten steps (0.18 m rise, 0.28 m run), up (`down` false) or down, played in
/// place while the character moves 15 % faster. Every frame is planned (with heel and toe
/// samples when `soles`), posed with `apply_foot_plant_with_toes`, and the solved
/// heel and toe tip are measured against the stairs.
fn stair_walk(soles: bool, down: bool) -> StairWalk {
    let settings = FootPlantSettings::default();
    let skeleton = demo_humanoid(&DemoRig::default()).expect("demo rig");
    let shape = demo_shape(&skeleton);
    let gait = DemoGait::standard()
        .into_iter()
        .find(|g| g.name == "walk")
        .expect("walk gait");
    let clip = demo_in_place(
        &demo_locomotion(&skeleton, std::slice::from_ref(&gait))
            .pop()
            .expect("walk clip"),
    );
    let legs: Vec<FootIkLeg> = DEMO_LEGS
        .iter()
        .map(|[hip, knee, ankle]| {
            FootIkLeg::new(IkChain {
                root: *hip as u32,
                mid: *knee as u32,
                tip: *ankle as u32,
            })
        })
        .collect();
    let toes = [Some(15_u32), Some(19)];
    let bind = skeleton.bind_pose_pose();
    let bind_globals = skeleton.bind_globals();
    let ankle_height = joint_position(&skeleton, &bind, DEMO_LEGS[0][2] as u32).y;
    let length: Vec<f32> = legs
        .iter()
        .map(|l| {
            let hip = joint_position(&skeleton, &bind, l.chain.root);
            let knee = joint_position(&skeleton, &bind, l.chain.mid);
            let ankle = joint_position(&skeleton, &bind, l.chain.tip);
            (knee - hip).length() + (ankle - knee).length()
        })
        .collect();
    // Both feet share the shape: identity bind frames, and the right foot mirrors the left
    // across x, which the shape's x = 0 points do not see.
    let tip_in_toe: Vec<Vec3> = DEMO_LEGS
        .iter()
        .zip(toes)
        .map(|(chain, toe)| {
            let foot = bind_globals[chain[2]];
            let toe = bind_globals[toe.expect("toe") as usize];
            toe.inverse().transform_point3(foot.transform_point3(shape.toe))
        })
        .collect();
    let (yaw, start, end) = if down {
        (std::f32::consts::PI, RUN * STEPS + 1.0, -1.0)
    } else {
        (0.0, -1.0, RUN * STEPS + 1.0)
    };
    let rotation = Quat::from_rotation_y(yaw);
    let speed = gait.speed * MISMATCH;
    let seconds = (end - start).abs() / speed;
    // The capsule rides the line through the tread middles.
    let rig_height = |s: f32| (RISE * (s / RUN + 0.5)).clamp(0.0, RISE * STEPS);

    let mut planter = FootPlanter::new(settings);
    let mut out = StairWalk::default();
    let frames = (seconds / DT) as usize;
    let settle = (0.5 / DT) as usize;
    for frame in 0..frames {
        let t = frame as f32 * DT;
        let s = start + (end - start).signum() * speed * t;
        let rig = Vec3::new(0.0, rig_height(s), -s);
        let mut pose = bind.clone();
        clip.sample_pose(t % gait.cycle, &mut pose);
        let to_world = Mat4::from_rotation_translation(rotation, rig);
        let globals = skeleton.globals(&pose, None);
        let mut inputs = Vec::with_capacity(2);
        let mut sole_in = Vec::with_capacity(2);
        for (i, leg) in legs.iter().enumerate() {
            let foot_world = to_world * globals[leg.chain.tip as usize];
            let foot = foot_world.w_axis.truncate();
            let probe = planter.pin(i).unwrap_or(foot);
            inputs.push(FootPlantLeg {
                hip: to_world.transform_point3(globals[leg.chain.root as usize].w_axis.truncate()),
                foot,
                length: length[i],
                ankle_height,
                ground: FootGround::Contact(Vec3::new(probe.x, stairs(probe), probe.z)),
                normal: Vec3::Y,
                contact: None,
            });
            if soles {
                let mut leg_shape = shape;
                leg_shape.ball = Some(
                    globals[leg.chain.tip as usize]
                        .inverse()
                        .transform_point3(globals[toes[i].expect("toe") as usize].w_axis.truncate()),
                );
                sole_in.push(sole_on(stairs, foot_world, probe, &leg_shape));
            }
        }
        let authored: Vec<bool> = legs
            .iter()
            .map(|l| (joint_position(&skeleton, &pose, l.chain.tip).y - ankle_height).abs() < 1.0e-3)
            .collect();
        let input = FootPlantInput {
            rig_translation: rig,
            rig_rotation: rotation,
            legs: &inputs,
            active: true,
            dt: DT,
        };
        let plan = planter.plan_with_soles(&input, &sole_in);
        let mut solved = pose.clone();
        apply_foot_plant_with_toes(&skeleton, &mut solved, &plan, &legs, &toes, Some(Vec3::NEG_Z));
        let solved_globals = skeleton.globals(&solved, None);
        for (i, leg) in legs.iter().enumerate() {
            if frame <= settle || !authored[i] || plan.legs[i].planted < 0.5 {
                continue;
            }
            let foot = to_world * solved_globals[leg.chain.tip as usize];
            let toe_joint = to_world * solved_globals[toes[i].expect("toe") as usize];
            let heel = foot.transform_point3(shape.heel);
            let toe = toe_joint.transform_point3(tip_in_toe[i]);
            let p = classify(heel, toe, settings.min_support);
            out.reach_gap = out
                .reach_gap
                .max((foot.w_axis.truncate() - to_world.transform_point3(plan.legs[i].target)).length());
            out.planted_frames += 1;
            out.penetration = out.penetration.max(-p.heel_gap).max(-p.toe_gap);
            out.float = out.float.max(p.heel_gap.min(p.toe_gap));
            if !(p.flush || p.resting) {
                out.unsupported += 1;
            }
            if stairs(heel).min(stairs(toe)) >= RISE * 2.0 - 1.0e-4 {
                out.on_stairs += 1;
            }
        }
    }
    out
}

/// Acceptance: the demo walk up and down ten steps keeps planted soles on the treads: no heel or
/// toe below the ground (penetration 0, within the two-bone solve's 0.1 mm) and no planted foot
/// more than 1 cm over the tread under it. Every planted leg reaches its target (reach gap
/// under 1 mm). The single ankle probe sinks a sole into a riser on
/// the same walk.
#[test]
fn stairs_up_and_down_keep_soles_on_treads() {
    for down in [false, true] {
        let single = stair_walk(false, down);
        let feet = stair_walk(true, down);
        println!("foot_stairs walk down={down} single_probe={single:?} heel_toe={feet:?}");
        assert!(
            feet.on_stairs > 20,
            "down={down}: the walk crossed the stairs {feet:?}"
        );
        assert!(
            single.penetration > 0.02,
            "down={down}: the scenario discriminates (single probe sinks {} m)",
            single.penetration
        );
        assert!(
            feet.penetration <= 1.0e-4,
            "down={down}: a planted sole went {} m under the stairs",
            feet.penetration
        );
        assert!(feet.reach_gap <= 1.0e-3, "down={down}: {feet:?}");
        assert!(
            feet.float <= 0.01,
            "down={down}: a planted foot floated {} m",
            feet.float
        );
        // A stance that runs out of leg reach moves the foot from the lower tread onto the higher
        // one (the stair rule never leaves a foot out of reach); that move eases over a few frames,
        // during which the sole stands on the nosing: never under a tread, never floating.
        assert!(
            feet.unsupported * 100 <= feet.planted_frames,
            "down={down}: {} of {} planted frames off a tread {feet:?}",
            feet.unsupported,
            feet.planted_frames
        );
    }
}

/// Acceptance: on a 5 to 25 degree ramp the foot pitch follows the heel and toe heights, up and
/// down the ramp, also when the probe's normal is flat (a coarse collision mesh, where the
/// single-probe planter keeps the sole level and digs the toe in).
#[test]
fn heel_toe_pitch_matches_a_ramp() {
    let settings = FootPlantSettings::default();
    let skeleton = demo_humanoid(&DemoRig::default()).expect("rig");
    let shape = demo_shape(&skeleton);
    for deg in [5.0_f32, 10.0, 15.0, 20.0, 25.0] {
        let tan = deg.to_radians().tan();
        let ramp = |p: Vec3| -p.z * tan;
        let true_normal = Vec3::new(0.0, 1.0, tan).normalize();
        for (yaw, normal) in [
            (0.0_f32, true_normal),
            (0.0, Vec3::Y),
            (std::f32::consts::PI, true_normal),
            (std::f32::consts::PI, Vec3::Y),
        ] {
            let rotation = Quat::from_rotation_y(yaw);
            let ankle_xz = Vec3::new(0.0, 0.0, -0.5);
            let rig = Vec3::new(0.0, ramp(ankle_xz), ankle_xz.z) - rotation * Vec3::new(-0.1, 0.0, 0.0);
            let to_world = Mat4::from_rotation_translation(rotation, rig);
            let foot = to_world * Mat4::from_translation(Vec3::new(-0.1, 0.08, 0.0));
            let ankle = foot.w_axis.truncate();
            let mut planter = FootPlanter::new(settings);
            let mut plan = FootPlant::default();
            for _ in 0..240 {
                let probe = planter.pin(0).unwrap_or(ankle);
                let leg = FootPlantLeg {
                    ground: FootGround::Contact(Vec3::new(probe.x, ramp(probe), probe.z)),
                    normal,
                    ..FootPlantLeg::new(
                        to_world.transform_point3(Vec3::new(-0.1, 0.98, 0.0)),
                        ankle,
                        0.9,
                        0.08,
                    )
                };
                let sole = sole_on(ramp, foot, probe, &shape);
                plan = planter.plan_with_soles(
                    &FootPlantInput {
                        rig_translation: rig,
                        rig_rotation: rotation,
                        legs: std::slice::from_ref(&leg),
                        active: true,
                        dt: DT,
                    },
                    &[sole],
                );
            }
            let leg = &plan.legs[0];
            let want = deg.to_radians().min(settings.max_tilt).to_degrees();
            let got = regia_animation::foot_ik::plant::tilt_angle(leg.tilt).to_degrees();
            // The sole line from heel to toe, in world space, has the ramp's slope.
            let placed = to_world.transform_point3(leg.target);
            let heel = placed + rotation * (leg.tilt * shape.heel);
            let toe = placed + rotation * (leg.tilt * shape.toe);
            let pitch = (toe.y - heel.y).atan2(Vec3::new(toe.x - heel.x, 0.0, toe.z - heel.z).length());
            println!(
                "foot_stairs ramp={deg} yaw={yaw:.2} normal_y={:.3} tilt={got:.3} sole_pitch={:.3} heel_gap={:.4} toe_gap={:.4}",
                normal.y,
                pitch.to_degrees(),
                heel.y - ramp(heel),
                toe.y - ramp(toe)
            );
            assert!(
                (got - want).abs() < 0.3,
                "ramp {deg} yaw {yaw}: tilt {got} want {want}"
            );
            assert!(
                (pitch.to_degrees().abs() - want).abs() < 0.3,
                "ramp {deg} yaw {yaw}: sole pitch {}",
                pitch.to_degrees()
            );
            for (name, p) in [("heel", heel), ("toe", toe)] {
                let g = p.y - ramp(p);
                assert!(
                    (-1.0e-4..=0.01).contains(&g),
                    "ramp {deg} yaw {yaw}: {name} gap {g}"
                );
            }
            assert!(leg.heel_gap.abs() <= 0.01 && leg.toe_gap.abs() <= 0.01, "{leg:?}");
        }
    }
}

/// Acceptance: while the clip lifts the heel about the ball during stance exit, the toe joint
/// counter-rotates so the toe tip stays on the ground (within 1 cm) with the ball; when the clip
/// lifts the foot the bend fades out.
#[test]
fn toe_bends_while_the_heel_lifts() {
    let skeleton = demo_humanoid(&DemoRig::default()).expect("rig");
    let shape = demo_shape(&skeleton);
    let ik = [
        FootIkLeg::new(IkChain {
            root: 12,
            mid: 13,
            tip: 14,
        }),
        FootIkLeg::new(IkChain {
            root: 16,
            mid: 17,
            tip: 18,
        }),
    ];
    let toes = [Some(15_u32), Some(19)];
    let bind = skeleton.bind_pose_pose();
    let bind_globals = skeleton.bind_globals();
    let tip_in_toe = bind_globals[15]
        .inverse()
        .transform_point3(bind_globals[14].transform_point3(shape.toe));
    let mut planter = FootPlanter::new(FootPlantSettings::default());
    let floor = |_: Vec3| 0.0;
    let pose_at = |lift_deg: f32, raise: f32| -> Pose {
        let mut pose = bind.clone();
        // Heel up about the ankle (the clip's own roll); the ankle rises by `raise`.
        pose.rotations[14] = Quat::from_rotation_x(-lift_deg.to_radians()) * pose.rotations[14];
        pose.translations[12].y += raise;
        pose
    };
    let plan_pose = |planter: &mut FootPlanter, pose: &Pose| -> FootPlant {
        let globals = skeleton.globals(pose, None);
        let mut legs = Vec::new();
        let mut soles = Vec::new();
        for (i, leg) in ik.iter().enumerate() {
            let foot = globals[leg.chain.tip as usize];
            let ankle = foot.w_axis.truncate();
            let probe = planter.pin(i).unwrap_or(ankle);
            legs.push(FootPlantLeg {
                ground: FootGround::Contact(Vec3::new(probe.x, 0.0, probe.z)),
                ..FootPlantLeg::new(
                    globals[leg.chain.root as usize].w_axis.truncate(),
                    ankle,
                    0.9,
                    0.08,
                )
            });
            let mut leg_shape = shape;
            leg_shape.ball = Some(
                foot.inverse()
                    .transform_point3(globals[toes[i].expect("toe") as usize].w_axis.truncate()),
            );
            soles.push(sole_on(floor, foot, probe, &leg_shape));
        }
        planter.plan_with_soles(
            &FootPlantInput {
                rig_translation: Vec3::ZERO,
                rig_rotation: Quat::IDENTITY,
                legs: &legs,
                active: true,
                dt: DT,
            },
            &soles,
        )
    };
    // Stand, then roll the left heel up to 40 degrees over 40 frames.
    for _ in 0..30 {
        plan_pose(&mut planter, &bind);
    }
    let mut largest_bend: f32 = 0.0;
    for k in 0..=40 {
        let lift = k as f32;
        let pose = pose_at(lift, 0.0);
        let plan = plan_pose(&mut planter, &pose);
        let mut solved = pose.clone();
        apply_foot_plant_with_toes(&skeleton, &mut solved, &plan, &ik, &toes, Some(Vec3::NEG_Z));
        let globals = skeleton.globals(&solved, None);
        let ball = globals[15].w_axis.truncate();
        let tip = globals[15].transform_point3(tip_in_toe);
        let bend = regia_animation::foot_ik::plant::tilt_angle(plan.legs[0].toe).to_degrees();
        largest_bend = largest_bend.max(bend);
        assert!(
            plan.legs[0].planted > 0.99,
            "lift {lift}: still planted {:?}",
            plan.legs[0]
        );
        for (name, p) in [("ball", ball), ("toe tip", tip)] {
            assert!(
                (-1.0e-3..=0.01).contains(&p.y),
                "lift {lift}: {name} at {} (bend {bend})",
                p.y
            );
        }
        assert!((bend - lift).abs() < 1.0, "lift {lift}: bend {bend}");
    }
    // The clip lifts the foot: the toe lifts and the bend fades out.
    let mut last = FootPlant::default();
    for k in 0..30 {
        last = plan_pose(&mut planter, &pose_at(40.0, 0.01 * k as f32));
    }
    println!(
        "foot_stairs toe: largest_bend={largest_bend:.2}deg released planted={:.3}",
        last.legs[0].planted
    );
    assert!(largest_bend > 39.0);
    assert!(last.legs[0].planted < 0.01, "{:?}", last.legs[0]);
    assert!(regia_animation::foot_ik::plant::tilt_angle(last.legs[0].toe) < 1.0e-4);
}

/// Acceptance: with no heel or toe samples the planter is the single-probe planter, bit for bit:
/// `plan`, `plan_with_soles` with no soles, with default soles and with half a sole (a probe that
/// missed) agree on every output bit over a varied run (moving and turning rig, slopes, swing,
/// contacts, fading out and in). `foot_plant.rs`, the single-probe planter's own acceptance,
/// runs unchanged.
#[test]
fn single_probe_input_is_bit_identical_to_f() {
    let half = FootSole {
        heel: probe_foot_sole(
            Mat4::from_translation(Vec3::new(0.1, 0.08, 0.0)),
            Vec3::new(0.1, 0.08, 0.0),
            &FootShape {
                heel: Vec3::new(0.0, -0.08, 0.06),
                toe: Vec3::new(0.0, -0.08, -0.17),
                ball: None,
            },
            0.04,
            5,
            |p| Some(Vec3::new(p.x, 0.0, p.z)),
        )
        .heel,
        ..FootSole::default()
    };
    assert!(half.heel.is_some());
    let mut planters: Vec<FootPlanter> = (0..4)
        .map(|_| FootPlanter::new(FootPlantSettings::default()))
        .collect();
    let mut checked = 0;
    for k in 0..300 {
        let phase = k as f32 * 0.05;
        let slope = |deg: f32| {
            let a = deg.to_radians();
            Vec3::new(-a.sin(), a.cos(), 0.0)
        };
        let mut legs = [-1.0_f32, 1.0].map(|side| {
            let foot = Vec3::new(0.197 * side, 0.128, 0.0);
            FootPlantLeg {
                ground: FootGround::Contact(Vec3::new(foot.x, 0.1 * (phase * 0.7).fract(), foot.z)),
                normal: slope(10.0 + phase),
                ..FootPlantLeg::new(Vec3::new(0.109 * side, 0.919, 0.03), foot, 0.8, 0.128)
            }
        });
        legs[0].foot.y = 0.128 + 0.1 * (phase.fract() - 0.5).max(0.0);
        legs[1].foot.z = 0.2 * (phase * 0.3).fract();
        if k % 50 > 40 {
            legs[1].contact = Some(if k % 2 == 0 { 1.0 } else { 0.0 });
        }
        let input = FootPlantInput {
            rig_translation: Vec3::new(phase, 0.0, -phase),
            rig_rotation: Quat::from_rotation_y(phase * 0.1),
            legs: &legs,
            active: k % 70 < 60,
            dt: DT,
        };
        let plans = [
            planters[0].plan(&input),
            planters[1].plan_with_soles(&input, &[]),
            planters[2].plan_with_soles(&input, &[FootSole::default(); 2]),
            planters[3].plan_with_soles(&input, &[half, half]),
        ];
        let bits = |plan: &FootPlant| {
            let mut v = vec![plan.pelvis_offset.to_bits(), plan.weight.to_bits()];
            for l in &plan.legs {
                v.extend(l.target.to_array().map(f32::to_bits));
                v.extend(l.tilt.to_array().map(f32::to_bits));
                v.extend(l.toe.to_array().map(f32::to_bits));
                v.extend([l.planted, l.lock, l.heel_gap, l.toe_gap].map(f32::to_bits));
            }
            v
        };
        let reference = bits(&plans[0]);
        for (i, plan) in plans.iter().enumerate().skip(1) {
            assert_eq!(bits(plan), reference, "frame {k}: entry point {i} differs");
        }
        for (i, planter) in planters.iter().enumerate().skip(1) {
            assert_eq!(
                planter.pin(0),
                planters[0].pin(0),
                "frame {k}: pin of planter {i}"
            );
            assert_eq!(planter.slide(1), planters[0].slide(1));
        }
        checked += 1;
    }
    assert_eq!(checked, 300);
}

/// The stairs as a scene probe for `FootPlacement`.
struct StairProbe;

impl regia_animation::foot_ik::GroundProbe for StairProbe {
    fn probe(
        &self,
        from: Vec3,
        down: Vec3,
        max_distance: f32,
        _ignore: regia_ecs::prelude::Entity,
    ) -> Option<regia_animation::foot_ik::GroundHit> {
        if down.y >= 0.0 || down.x != 0.0 || down.z != 0.0 {
            return None;
        }
        let y = stairs(from);
        (from.y - y >= 0.0 && from.y - y <= max_distance).then_some(regia_animation::foot_ik::GroundHit {
            point: Vec3::new(from.x, y, from.z),
            normal: Vec3::Y,
        })
    }
}

/// Agent surface: a rig with `FootPlacement::foot_shape` standing with both ankles right over a
/// riser gets heel and toe probes from the engine, and the reflected `FootPlantReport` carries
/// `heel_gap` and `toe_gap` (both at most 0.01, never under the ground), read by path as
/// `scene.get_entity` and `play.expect` read them. Without `foot_shape` both stay 0.
#[test]
fn foot_placement_reports_heel_and_toe_gaps_on_stairs() {
    use regia_animation::components::spawn_skeleton;
    use regia_animation::demo_rig::RigNaming;
    use regia_animation::foot_ik::{FootLegDesc, FootPlacement, FootPlantReport, GroundProbes};
    use regia_animation::{AnimationPlugin, Animator};
    use regia_app::prelude::*;
    use regia_ecs::reflect::{GetPath, ReflectComponent};
    use regia_scene::Transform;
    use std::sync::Arc;
    use std::time::Duration;

    let skeleton = Arc::new(
        demo_humanoid(&DemoRig {
            naming: RigNaming::Game,
            ..DemoRig::default()
        })
        .expect("rig"),
    );
    let idle = DemoGait::standard()
        .into_iter()
        .find(|g| g.name == "idle")
        .expect("idle gait");
    let clip = demo_locomotion(&skeleton, std::slice::from_ref(&idle))
        .pop()
        .expect("idle clip");
    let legs = ["l", "r"].map(|side| FootLegDesc {
        hip: format!("thigh_{side}"),
        knee: format!("calf_{side}"),
        ankle: format!("foot_{side}"),
        toe: Some(format!("ball_{side}")),
        pole_offset: Vec3::ZERO,
    });
    for shaped in [true, false] {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AnimationPlugin));
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(DT)));
        app.insert_resource(GroundProbes::new(StairProbe));
        let mut placement = FootPlacement::default();
        placement.legs = legs.to_vec();
        if shaped {
            placement.foot_shape = Some(FootShapeSettings::default());
        }
        let binding = spawn_skeleton(app.world_mut(), skeleton.clone());
        let transform = Transform {
            translation: Vec3::new(0.0, 0.27, -RUN),
            ..Transform::default()
        };
        let rig = app
            .world_mut()
            .spawn((
                transform,
                binding,
                Animator::single(clip.clone(), true),
                placement,
            ))
            .id();
        for _ in 0..90 {
            app.update();
        }
        let report = app.world().get::<FootPlantReport>(rig).expect("report").clone();
        let registry = app.world().resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let component = registry
            .get_with_short_type_path("FootPlantReport")
            .and_then(|r| r.data::<ReflectComponent>())
            .expect("FootPlantReport is a reflected component");
        let reflected = component.reflect(app.world().entity(rig)).expect("reflect");
        let read = |path: &str| -> f32 {
            *reflected
                .reflect_path(path)
                .ok()
                .and_then(|f| f.try_downcast_ref::<f32>())
                .unwrap_or_else(|| panic!("{path} is a reflected f32"))
        };
        println!("foot_stairs component shaped={shaped} report={report:?}");
        assert_eq!(report.heel_gap.len(), 2);
        assert_eq!(report.toe_gap.len(), 2);
        for leg in 0..2 {
            assert!(report.planted[leg] > 0.99, "{report:?}");
            let (heel, toe) = (
                read(&format!("heel_gap[{leg}]")),
                read(&format!("toe_gap[{leg}]")),
            );
            assert_eq!(heel, report.heel_gap[leg]);
            assert_eq!(toe, report.toe_gap[leg]);
            if shaped {
                assert!((-1.0e-3..=0.01).contains(&heel), "leg {leg}: heel_gap {heel}");
                assert!((-1.0e-3..=0.01).contains(&toe), "leg {leg}: toe_gap {toe}");
            } else {
                assert_eq!((heel, toe), (0.0, 0.0));
            }
        }
    }
}
