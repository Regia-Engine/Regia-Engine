// SPDX-License-Identifier: MPL-2.0
//! Foot planting (P-10, ADR-8088): the plant lock, tilt and smoothing of `foot_ik::FootPlanter` on the
//! demo walk.
//!
//! The walk is played as an in-place clip (`demo_in_place`: the planted feet slide back at the authored
//! 1.3 m/s) while the character moves 15 % faster, the classic speed mismatch that makes a clip skate.
//! Every frame is planned with [`FootPlanter::plan`] and posed with [`apply_foot_plant`]; the solved ankle
//! joints are measured in world space.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_animation::demo_rig::{
    DEMO_LEGS, DemoGait, DemoRig, demo_humanoid, demo_in_place, demo_locomotion,
};
use regia_animation::foot_ik::{
    FootGround, FootIkLeg, FootPlantInput, FootPlantLeg, FootPlantSettings, FootPlanter, apply_foot_plant,
};
use regia_animation::ik::{IkChain, joint_position};
use regia_core::math::{Quat, Vec3};

const DT: f32 = 1.0 / 60.0;
/// Character speed over the authored walk speed.
const MISMATCH: f32 = 1.15;

/// Per stance: the largest horizontal distance of the solved ankle from where it touched down.
struct Walk {
    /// Largest slide of a solved planted ankle over any stance, meters.
    solved_slide: f32,
    /// Largest slide of the planner's world target over any stance, meters.
    target_slide: f32,
    /// Stances measured.
    stances: usize,
    /// Largest per-frame jump of a solved ankle, meters (release pops show here).
    largest_step: f32,
    /// Largest vertical error of a solved planted ankle against the ground plus ankle height.
    planted_height_error: f32,
    /// Largest distance of a solved planted ankle from the planner's target (the leg could not reach).
    reach_gap: f32,
    /// Lowest pelvis offset the planner chose, meters.
    pelvis_min: f32,
}

fn horizontal(a: Vec3, b: Vec3) -> f32 {
    Vec3::new(a.x - b.x, 0.0, a.z - b.z).length()
}

/// Walks the demo character for `seconds` over flat ground (`slope_deg` 0) or a slope rising along
/// −Z, rotated by `yaw` and placed at `origin`, and measures the planted ankles.
fn walk(settings: FootPlantSettings, slope_deg: f32, yaw: f32, origin: Vec3, seconds: f32) -> Walk {
    let skeleton = demo_humanoid(&DemoRig::default()).expect("demo rig");
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
    let bind = skeleton.bind_pose_pose();
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
    let rotation = Quat::from_rotation_y(yaw);
    let forward = rotation * Vec3::NEG_Z;
    let tan = slope_deg.to_radians().tan();
    // Ground height at a world point: `origin.y`, rising along the walk direction.
    let rise = |p: Vec3| tan * (p - origin).dot(forward);
    let ground = |p: Vec3| origin.y + rise(p);
    let up_slope = (Vec3::Y - forward * tan).normalize();

    let mut planter = FootPlanter::new(settings);
    let mut out = Walk {
        solved_slide: 0.0,
        target_slide: 0.0,
        stances: 0,
        largest_step: 0.0,
        planted_height_error: 0.0,
        reach_gap: 0.0,
        pelvis_min: 0.0,
    };
    let mut touchdown: [Option<(Vec3, Vec3)>; 2] = [None; 2];
    let mut previous: [Option<Vec3>; 2] = [None; 2];
    let frames = (seconds / DT) as usize;
    let settle = (0.5 / DT) as usize;
    for frame in 0..frames {
        let t = frame as f32 * DT;
        let traveled = gait.speed * MISMATCH * t;
        let rig = origin + forward * traveled + Vec3::Y * rise(origin + forward * traveled);
        let mut pose = bind.clone();
        clip.sample_pose(t % gait.cycle, &mut pose);
        let to_world = |p: Vec3| rotation * p + rig;
        let inputs: Vec<FootPlantLeg> = legs
            .iter()
            .enumerate()
            .zip(&length)
            .map(|((i, leg), &length)| {
                let foot = to_world(joint_position(&skeleton, &pose, leg.chain.tip));
                // While a pin holds the foot, probe the ground under the pin.
                let probe = planter.pin(i).unwrap_or(foot);
                FootPlantLeg {
                    hip: to_world(joint_position(&skeleton, &pose, leg.chain.root)),
                    foot,
                    length,
                    ankle_height,
                    ground: FootGround::Contact(Vec3::new(probe.x, ground(probe), probe.z)),
                    normal: up_slope,
                    contact: None,
                }
            })
            .collect();
        // Authored planted frames: the in-place clip holds the planted ankle at ankle height.
        let authored: Vec<bool> = legs
            .iter()
            .map(|l| (joint_position(&skeleton, &pose, l.chain.tip).y - ankle_height).abs() < 1.0e-3)
            .collect();
        let plan = planter.plan(&FootPlantInput {
            rig_translation: rig,
            rig_rotation: rotation,
            legs: &inputs,
            active: true,
            dt: DT,
        });
        let mut solved = pose.clone();
        apply_foot_plant(&skeleton, &mut solved, &plan, &legs, Some(Vec3::NEG_Z));
        for (i, leg) in legs.iter().enumerate() {
            let ankle = to_world(joint_position(&skeleton, &solved, leg.chain.tip));
            let target = to_world(plan.legs[i].target);
            if frame > settle
                && let Some(prev) = previous[i]
            {
                out.largest_step = out.largest_step.max((ankle - prev).length());
            }
            previous[i] = Some(ankle);
            if frame <= settle || !authored[i] {
                touchdown[i] = None;
                continue;
            }
            let (ankle0, target0) = *touchdown[i].get_or_insert_with(|| {
                out.stances += 1;
                (ankle, target)
            });
            out.solved_slide = out.solved_slide.max(horizontal(ankle, ankle0));
            out.reach_gap = out.reach_gap.max((ankle - target).length());
            out.pelvis_min = out.pelvis_min.min(plan.pelvis_offset);
            out.target_slide = out.target_slide.max(horizontal(target, target0));
            if slope_deg == 0.0 {
                out.planted_height_error = out
                    .planted_height_error
                    .max((ankle.y - (ground(ankle) + ankle_height)).abs());
            } else if authored[i] {
                let cos = slope_deg.to_radians().cos().max(0.5);
                let signed = ankle.y - (ground(ankle) + ankle_height / cos);
                out.planted_height_error = out.planted_height_error.max(signed.abs());
            }
        }
    }
    out
}

/// P-10 acceptance: with the plant lock a planted foot never slides more than 1 cm over the ground,
/// although the clip under it skates ~10 cm per stance.
#[test]
fn planted_foot_never_slides_over_1cm() {
    let skating = walk(
        FootPlantSettings {
            lock: false,
            ..FootPlantSettings::default()
        },
        0.0,
        0.0,
        Vec3::ZERO,
        3.0,
    );
    println!(
        "foot_plant lock=off stances={} solved_slide={:.4}m target_slide={:.4}m",
        skating.stances, skating.solved_slide, skating.target_slide
    );
    assert!(skating.stances >= 4, "the walk has stances: {}", skating.stances);
    assert!(
        skating.solved_slide > 0.05,
        "the scenario skates without the lock: {}",
        skating.solved_slide
    );

    for (yaw, origin) in [(0.0_f32, Vec3::ZERO), (2.1, Vec3::new(40.0, 3.0, -17.0))] {
        let locked = walk(FootPlantSettings::default(), 0.0, yaw, origin, 3.0);
        println!(
            "foot_plant lock=on yaw={yaw} stances={} solved_slide={:.4}m target_slide={:.4}m largest_step={:.4}m height_error={:.4}m reach_gap={:.4}m pelvis_min={:.4}m",
            locked.stances,
            locked.solved_slide,
            locked.target_slide,
            locked.largest_step,
            locked.planted_height_error,
            locked.reach_gap,
            locked.pelvis_min
        );
        assert!(locked.stances >= 4);
        assert!(
            locked.target_slide <= 0.01,
            "yaw {yaw}: planner target slid {} m",
            locked.target_slide
        );
        assert!(
            locked.solved_slide <= 0.01,
            "yaw {yaw}: solved ankle slid {} m",
            locked.solved_slide
        );
        assert!(
            locked.planted_height_error <= 0.01,
            "yaw {yaw}: planted ankle {} m off the ground",
            locked.planted_height_error
        );
    }
}

/// The lock hands the foot back to the clip without a pop: no solved ankle moves farther in one frame
/// than the swing itself does without the lock (plus a margin for the release blend).
#[test]
fn releasing_the_lock_does_not_pop_the_foot() {
    let free = walk(
        FootPlantSettings {
            lock: false,
            ..FootPlantSettings::default()
        },
        0.0,
        0.0,
        Vec3::ZERO,
        3.0,
    );
    let locked = walk(FootPlantSettings::default(), 0.0, 0.0, Vec3::ZERO, 3.0);
    println!(
        "foot_plant largest_step free={:.4}m locked={:.4}m",
        free.largest_step, locked.largest_step
    );
    assert!(
        locked.largest_step <= free.largest_step * 1.5 + 0.005,
        "release pop: {} vs {}",
        locked.largest_step,
        free.largest_step
    );
}

/// On a 15 degree slope the locked feet still hold, both uphill and with the rig turned.
#[test]
fn the_lock_holds_on_a_slope() {
    for yaw in [0.0_f32, -1.3] {
        let locked = walk(
            FootPlantSettings::default(),
            15.0,
            yaw,
            Vec3::new(-5.0, 1.0, 2.0),
            3.0,
        );
        println!(
            "foot_plant slope=15 yaw={yaw} stances={} solved_slide={:.4}m target_slide={:.4}m reach_gap={:.4}m pelvis_min={:.4}m sole={:.4}m",
            locked.stances,
            locked.solved_slide,
            locked.target_slide,
            locked.reach_gap,
            locked.pelvis_min,
            locked.planted_height_error
        );
        assert!(locked.stances >= 4);
        assert!(locked.target_slide <= 0.01, "yaw {yaw}: {}", locked.target_slide);
        assert!(locked.solved_slide <= 0.015, "yaw {yaw}: {}", locked.solved_slide);
        assert!(
            locked.planted_height_error <= 0.01,
            "yaw {yaw}: planted sole {} m off the ground",
            locked.planted_height_error
        );
    }
}

/// The agent anchor: `AnimationPlugin` registers `FootPlantReport` as a reflected component, and its
/// fields read by path (what `scene.get_entity` and `play.expect` do) after a host inserted it from a
/// skating walk with the lock on.
#[test]
fn the_report_is_a_reflected_component_agents_can_read() {
    use regia_app::prelude::*;
    use regia_ecs::reflect::{GetPath, ReflectComponent};

    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, regia_animation::AnimationPlugin));

    // A two-leg body standing on flat ground while its clip feet skate backward.
    let mut planter = FootPlanter::new(FootPlantSettings::default());
    let mut legs = [-1.0_f32, 1.0].map(|side| {
        let foot = Vec3::new(0.1 * side, 0.08, 0.0);
        FootPlantLeg {
            ground: FootGround::Contact(Vec3::new(foot.x, 0.0, foot.z)),
            ..FootPlantLeg::new(Vec3::new(0.1 * side, 0.9, 0.0), foot, 0.85, 0.08)
        }
    });
    let mut plan = planter.plan(&FootPlantInput {
        rig_translation: Vec3::ZERO,
        rig_rotation: Quat::IDENTITY,
        legs: &legs,
        active: true,
        dt: DT,
    });
    for _ in 0..30 {
        for leg in &mut legs {
            leg.foot.z += 0.004;
        }
        plan = planter.plan(&FootPlantInput {
            rig_translation: Vec3::ZERO,
            rig_rotation: Quat::IDENTITY,
            legs: &legs,
            active: true,
            dt: DT,
        });
    }
    let report = planter.report(&plan);
    assert_eq!(report.lock, vec![1.0, 1.0]);
    assert!(report.max_slide <= 0.01, "{report:?}");
    let rig = app.world_mut().spawn(report.clone()).id();

    let registry = app.world().resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let registration = registry
        .get_with_short_type_path("FootPlantReport")
        .expect("FootPlantReport is registered");
    let component = registration
        .data::<ReflectComponent>()
        .expect("FootPlantReport is a reflected component");
    let reflected = component.reflect(app.world().entity(rig)).expect("reflect");
    let read = |path: &str| -> f32 {
        *reflected
            .reflect_path(path)
            .ok()
            .and_then(|f| f.try_downcast_ref::<f32>())
            .unwrap_or_else(|| panic!("{path} is a reflected f32"))
    };
    assert_eq!(read("max_slide"), report.max_slide);
    assert_eq!(read("weight"), report.weight);
    assert_eq!(read("lock[0]"), 1.0);
    assert_eq!(read("planted[1]"), 1.0);
    assert_eq!(read("tilt_deg[0]"), report.tilt_deg[0]);
}
