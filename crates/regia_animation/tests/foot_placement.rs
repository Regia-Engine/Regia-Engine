// SPDX-License-Identifier: MPL-2.0
//! `FootPlacement` (ADR-8388): the engine probes, plans and applies. These tests fail to compile
//! on the foot-planting base, which has no component.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use regia_animation::clip::{AnimationClip, NamedCurve, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::demo_rig::{
    DemoGait, DemoRig, RigNaming, demo_humanoid, demo_in_place, demo_locomotion,
};
use regia_animation::foot_ik::{
    FootLegDesc, FootPlacement, FootPlacementOutput, FootPlacementRespawn, FootPlacementStatus,
    FootPlantReport, GroundHit, GroundProbe, GroundProbes,
};
use regia_animation::graph::{AnimGraph, AnimNode, ClipNode, IkTarget, TwoBoneIKNode};
use regia_animation::montage::Montage;
use regia_animation::skeleton::LocalBind;
use regia_animation::{
    AnimationPlugin, Animator, ClipLibrary, JointGlobals, MontagePlayer, PlayOptions, Skeleton, SlotDef,
};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;

const DT: f32 = 1.0 / 60.0;
const MISMATCH: f32 = 1.15;
const WALK_SPEED: f32 = 1.3;

/// Analytic slope. Height at `p` is `origin.y + tan * (p - origin) · forward`.
#[derive(Clone, Copy)]
struct SlopeProbe {
    origin: Vec3,
    forward: Vec3,
    tan: f32,
    normal: Vec3,
}

impl SlopeProbe {
    fn new(origin: Vec3, yaw: f32, slope_deg: f32) -> Self {
        let forward = Quat::from_rotation_y(yaw) * Vec3::NEG_Z;
        let tan = slope_deg.to_radians().tan();
        let normal = (Vec3::Y - forward * tan).normalize();
        Self {
            origin,
            forward,
            tan,
            normal,
        }
    }

    fn ground(&self, p: Vec3) -> f32 {
        self.origin.y + self.tan * (p - self.origin).dot(self.forward)
    }
}

impl GroundProbe for SlopeProbe {
    fn probe(&self, from: Vec3, down: Vec3, max_distance: f32, _ignore: Entity) -> Option<GroundHit> {
        let down = down.normalize_or_zero();
        if down == Vec3::ZERO {
            return None;
        }
        let denom = down.dot(self.normal);
        if denom.abs() < 1.0e-8 {
            return None;
        }
        let t = (self.origin - from).dot(self.normal) / denom;
        if !t.is_finite() || t < 0.0 || t > max_distance {
            return None;
        }
        Some(GroundHit {
            point: from + down * t,
            normal: self.normal,
        })
    }
}

fn game_rig() -> DemoRig {
    DemoRig {
        naming: RigNaming::Game,
        ..DemoRig::default()
    }
}

fn walk_clip() -> (Arc<Skeleton>, AnimationClip) {
    // Game names (`thigh_l`, `foot_l`, `ball_l`). `DemoRig::default` is the plain set.
    let skeleton = demo_humanoid(&game_rig()).expect("demo rig");
    let gait = DemoGait::standard()
        .into_iter()
        .find(|gait| gait.name == "walk")
        .expect("walk gait");
    let clip = demo_in_place(
        &demo_locomotion(&skeleton, std::slice::from_ref(&gait))
            .pop()
            .expect("walk clip"),
    );
    (Arc::new(skeleton), clip)
}

fn demo_legs() -> Vec<FootLegDesc> {
    vec![
        FootLegDesc {
            hip: "thigh_l".into(),
            knee: "calf_l".into(),
            ankle: "foot_l".into(),
            toe: Some("ball_l".into()),
            pole_offset: Vec3::ZERO,
        },
        FootLegDesc {
            hip: "thigh_r".into(),
            knee: "calf_r".into(),
            ankle: "foot_r".into(),
            toe: Some("ball_r".into()),
            pole_offset: Vec3::ZERO,
        },
    ]
}

fn placement() -> FootPlacement {
    let mut placement = FootPlacement::default();
    placement.legs = demo_legs();
    placement
}

fn app_with(probe: impl GroundProbe + 'static) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(DT)));
    app.insert_resource(GroundProbes::new(probe));
    app
}

fn spawn_rig(
    app: &mut App,
    skeleton: Arc<Skeleton>,
    clip: AnimationClip,
    feet: Option<FootPlacement>,
) -> Entity {
    let animator = Animator::single(clip, true);
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let mut entity = app.world_mut().spawn((Transform::default(), binding, animator));
    if let Some(feet) = feet {
        entity.insert(feet);
    }
    entity.id()
}

fn set_transform(app: &mut App, rig: Entity, translation: Vec3, rotation: Quat) {
    let mut transform = app.world_mut().get_mut::<Transform>(rig).expect("transform");
    transform.translation = translation;
    transform.rotation = rotation;
    transform.scale = Vec3::ONE;
}

fn report(app: &App, rig: Entity) -> FootPlantReport {
    app.world()
        .get::<FootPlantReport>(rig)
        .expect("FootPlantReport")
        .clone()
}

fn ankle_world(app: &App, rig: Entity, ankle: usize) -> Vec3 {
    let globals = app.world().get::<JointGlobals>(rig).expect("globals");
    let local = globals.globals[ankle].w_axis.truncate();
    let transform = app.world().get::<Transform>(rig).expect("transform");
    transform.translation + transform.rotation * (local * transform.scale)
}

/// Walks the demo rig on a 15 degree slope. Planted soles stay within 1 cm of the ground and
/// `max_slide` stays within 1 cm.
#[test]
fn foot_placement_component_plants_on_a_15_degree_slope() {
    let (skeleton, clip) = walk_clip();
    let yaw = 0.4;
    let origin = Vec3::new(1.0, 0.25, -2.0);
    let slope = SlopeProbe::new(origin, yaw, 15.0);
    let theta = 15.0_f32.to_radians();
    let rotation = Quat::from_rotation_y(yaw);
    let forward = rotation * Vec3::NEG_Z;
    let bind = skeleton.bind_pose_pose();
    let ankle_ids = [
        skeleton.joint_index("foot_l").expect("foot_l"),
        skeleton.joint_index("foot_r").expect("foot_r"),
    ];
    // Object-space bind height, the same value the planter lifts by (`ankle / cos(tilt)`).
    // The foot joint's local translation is the offset from the calf, not that height.
    let ankle_height = skeleton.globals(&bind, None)[ankle_ids[0] as usize].w_axis.y;
    let mut app = app_with(slope);
    let rig = spawn_rig(&mut app, skeleton, clip, Some(placement()));
    let settle = (0.5 / DT) as usize;
    let frames = (2.0 / DT) as usize;
    let mut planted_samples = 0;
    let mut worst_sole = 0.0_f32;
    let mut worst_signed = 0.0_f32;
    let mut worst_tilt = 0.0_f32;
    let mut worst_pelvis = 0.0_f32;
    let mut worst_weight = 0.0_f32;
    let mut worst_above = 0.0_f32;
    let mut best_abs = f32::MAX;
    let mut worst_slide = 0.0_f32;
    let mut bad = 0_u32;
    let mut bad_note = String::new();
    for frame in 0..frames {
        let t = frame as f32 * DT;
        let travelled = WALK_SPEED * MISMATCH * t;
        let along = origin + forward * travelled;
        let rig_pos = Vec3::new(along.x, slope.ground(along), along.z);
        set_transform(&mut app, rig, rig_pos, rotation);
        app.update();
        let report = report(&app, rig);
        if frame <= settle {
            continue;
        }
        worst_slide = worst_slide.max(report.max_slide);
        for (leg, &ankle) in ankle_ids.iter().enumerate() {
            // Fully planted. Between `plant_lift` and `swing_lift` the planter keeps the clip's
            // own lift (`held`), so a half-planted ankle is above the ground on purpose.
            if report.planted.get(leg).copied().unwrap_or(0.0) < 0.99 {
                continue;
            }
            let ankle = ankle_world(&app, rig, ankle as usize);
            let sole = ankle.y - ankle_height / theta.cos();
            let signed = sole - slope.ground(ankle);
            let error = signed.abs();
            best_abs = best_abs.min(error);
            if error >= worst_sole {
                worst_sole = error;
                worst_signed = signed;
                worst_tilt = report.tilt_deg.get(leg).copied().unwrap_or(-1.0);
                worst_pelvis = report.pelvis_offset;
                worst_weight = report.weight;
                worst_above = ankle.y - slope.ground(ankle);
            }
            if error > 0.01 {
                bad += 1;
                if bad <= 4 {
                    let lock = report.lock.get(leg).copied().unwrap_or(-1.0);
                    let planted_w = report.planted.get(leg).copied().unwrap_or(-1.0);
                    let slide = report.slide.get(leg).copied().unwrap_or(-1.0);
                    bad_note.push_str(&format!(
                        " frame {frame} leg {leg} signed {signed:.4} lock {lock:.2} planted {planted_w:.3} slide {slide:.4};"
                    ));
                }
            }
            planted_samples += 1;
        }
    }
    assert!(
        planted_samples > 10,
        "expected planted frames, saw {planted_samples}"
    );
    assert!(
        worst_slide <= 0.01,
        "max_slide {worst_slide} over 1 cm ({planted_samples} planted samples)"
    );
    assert!(
        worst_sole <= 0.01,
        "sole error {worst_sole} m signed {worst_signed} tilt {worst_tilt} pelvis {worst_pelvis} weight {worst_weight} above {worst_above} best_abs {best_abs} bad {bad}{bad_note} ankle_height {ankle_height} ({planted_samples} planted samples)"
    );
    let status = app.world().get::<FootPlacementStatus>(rig).expect("status");
    assert!(status.probe_installed && status.legs_resolved);
}

#[test]
fn weight_curve_zero_fades_foot_placement_out() {
    let (skeleton, clip) = walk_clip();
    let mut off = clip.clone();
    off.name = "plant-off".into();
    off.curves.push(NamedCurve {
        name: "FootPlacementWeight".into(),
        curve: TrackCurve::Static(0.0),
    });
    let mut library = ClipLibrary::new();
    library.insert(off);
    let player = MontagePlayer::new(vec![SlotDef::full_body("FullBody", "full")], library);

    let mut app = app_with(SlopeProbe::new(Vec3::ZERO, 0.0, 0.0));
    let planted = spawn_rig(&mut app, skeleton.clone(), clip.clone(), Some(placement()));
    let plain = spawn_rig(&mut app, skeleton, clip, None);
    app.world_mut().entity_mut(planted).insert(player.clone());
    app.world_mut().entity_mut(plain).insert(player);

    // Fade in at blend_in_per_s (5). Twelve frames at 60 Hz reach 1.
    for _ in 0..12 {
        app.update();
    }
    let full = report(&app, planted).weight;
    assert!((full - 1.0).abs() < 1.0e-4, "weight after blend-in was {full}");

    app.world_mut()
        .get_mut::<Animator>(planted)
        .expect("animator")
        .set("FootPlacementWeight", 0.0);
    app.update();
    let faded = report(&app, planted).weight;
    let expect = (full - 10.0 * DT).max(0.0);
    assert!(
        (faded - expect).abs() < 1.0e-4,
        "one fade-out frame went from {full} to {faded}, expected {expect}"
    );
    for _ in 0..12 {
        if report(&app, planted).weight == 0.0 {
            break;
        }
        app.update();
    }
    assert_eq!(report(&app, planted).weight, 0.0);
    assert_eq!(
        app.world().get::<JointGlobals>(planted).expect("planted").globals,
        app.world().get::<JointGlobals>(plain).expect("plain").globals,
        "a faded planter must leave the clip pose"
    );

    // Back in, at blend_in_per_s.
    app.world_mut()
        .get_mut::<Animator>(planted)
        .expect("animator")
        .set("FootPlacementWeight", 1.0);
    let before = report(&app, planted).weight;
    app.update();
    let risen = report(&app, planted).weight;
    let expect = (before + 5.0 * DT).min(1.0);
    assert!(
        (risen - expect).abs() < 1.0e-4,
        "one fade-in frame went from {before} to {risen}, expected {expect}"
    );
    for _ in 0..20 {
        if report(&app, planted).weight == 1.0 {
            break;
        }
        app.update();
    }
    assert_eq!(report(&app, planted).weight, 1.0);

    // A montage curve at 0 overrides the parameter and, after the fade, matches a twin.
    for rig in [planted, plain] {
        let mut montage = Montage::single("off", "FullBody", "plant-off");
        montage.blend_in.time = 0.0;
        montage.blend_out.time = 0.0;
        let mut player = app.world_mut().get_mut::<MontagePlayer>(rig).expect("player");
        let montage = player.add_montage(montage);
        player.play(montage, PlayOptions::default());
    }
    for _ in 0..12 {
        app.update();
        if report(&app, planted).weight == 0.0 {
            break;
        }
    }
    assert_eq!(
        report(&app, planted).weight,
        0.0,
        "montage curve 0 fades the effect out"
    );
    assert_eq!(
        app.world().get::<JointGlobals>(planted).expect("planted").globals,
        app.world().get::<JointGlobals>(plain).expect("plain").globals,
        "after the montage fade the pose matches the clip"
    );
}

#[test]
fn airborne_character_fades_out() {
    let (skeleton, clip) = walk_clip();
    let mut app = app_with(SlopeProbe::new(Vec3::ZERO, 0.0, 0.0));
    let mut feet = placement();
    feet.grounded = Some(true);
    let rig = spawn_rig(&mut app, skeleton, clip, Some(feet));
    for _ in 0..12 {
        app.update();
    }
    let full = report(&app, rig).weight;
    assert!((full - 1.0).abs() < 1.0e-4, "weight after blend-in was {full}");
    app.world_mut()
        .get_mut::<FootPlacement>(rig)
        .expect("feet")
        .grounded = Some(false);
    let before = report(&app, rig).weight;
    app.update();
    let faded = report(&app, rig).weight;
    let expect = (before - 10.0 * DT).max(0.0);
    assert!(
        (faded - expect).abs() < 1.0e-4,
        "leaving the ground went from {before} to {faded}, expected {expect}"
    );
    for _ in 0..12 {
        if report(&app, rig).weight == 0.0 {
            break;
        }
        app.update();
    }
    assert_eq!(report(&app, rig).weight, 0.0);
}

#[test]
fn report_is_inserted_every_frame() {
    let (skeleton, clip) = walk_clip();
    let mut app = app_with(SlopeProbe::new(Vec3::ZERO, 0.0, 0.0));
    let rig = spawn_rig(&mut app, skeleton, clip, Some(placement()));
    for frame in 1..=8 {
        app.update();
        let report = app.world().get::<FootPlantReport>(rig);
        assert!(report.is_some(), "frame {frame} has no FootPlantReport");
        let status = app.world().get::<FootPlacementStatus>(rig).expect("status");
        assert!(status.probe_installed, "frame {frame}");
        assert!(status.legs_resolved, "frame {frame}");
        assert!(report.expect("report").weight.is_finite());
    }
}

#[test]
fn teleport_resets_the_pins() {
    let (skeleton, clip) = walk_clip();
    let mut app = app_with(SlopeProbe::new(Vec3::ZERO, 0.0, 0.0));
    let rig = spawn_rig(&mut app, skeleton, clip, Some(placement()));
    for _ in 0..30 {
        app.update();
    }
    assert_eq!(report(&app, rig).weight, 1.0);
    // A normal frame does not drop the effect.
    let before = app.world().get::<Transform>(rig).expect("transform").translation;
    set_transform(&mut app, rig, before + Vec3::X * 0.02, Quat::IDENTITY);
    app.update();
    assert_eq!(report(&app, rig).weight, 1.0, "a step must not clear the pins");

    set_transform(&mut app, rig, before + Vec3::X * 10.0, Quat::IDENTITY);
    app.update();
    let after_jump = report(&app, rig).weight;
    let expect = (5.0 * DT).min(1.0);
    assert!(
        (after_jump - expect).abs() < 1.0e-4,
        "a 10 m jump left weight {after_jump}, expected a fresh blend-in {expect}"
    );

    for _ in 0..20 {
        app.update();
    }
    assert_eq!(report(&app, rig).weight, 1.0);
    app.world_mut().entity_mut(rig).insert(FootPlacementRespawn);
    app.update();
    let after_respawn = report(&app, rig).weight;
    assert!(
        (after_respawn - expect).abs() < 1.0e-4,
        "respawn left weight {after_respawn}, expected {expect}"
    );
    assert!(app.world().get::<FootPlacementRespawn>(rig).is_none());
}

#[test]
fn rig_without_foot_placement_is_bit_identical() {
    let (skeleton, clip) = walk_clip();
    // The planting rig stands 15 cm above the plane the clip was authored on, so its ankles
    // move. The plain rig must still match the clip sample.
    let mut app = app_with(SlopeProbe::new(Vec3::Y * 0.15, 0.0, 0.0));
    let plain = spawn_rig(&mut app, skeleton.clone(), clip.clone(), None);
    let planted = spawn_rig(&mut app, skeleton.clone(), clip.clone(), Some(placement()));
    let frames = 20;
    let mut time = 0.0_f32;
    for _ in 0..frames {
        app.update();
        time = clip.step(time, DT, true).time;
    }
    let mut pose = skeleton.bind_pose_pose();
    clip.sample_pose(time, &mut pose);
    let sampled = skeleton.globals(&pose, None);
    let plain_globals = app
        .world()
        .get::<JointGlobals>(plain)
        .expect("plain")
        .globals
        .clone();
    assert_eq!(
        plain_globals, sampled,
        "a rig with no FootPlacement matches the clip sample"
    );
    let planted_ankle = ankle_world(
        &app,
        planted,
        skeleton.joint_index("foot_l").expect("foot") as usize,
    );
    let plain_ankle = ankle_world(
        &app,
        plain,
        skeleton.joint_index("foot_l").expect("foot") as usize,
    );
    assert!(
        (planted_ankle - plain_ankle).length() > 0.01,
        "the planting rig must actually move a foot (delta {})",
        (planted_ankle - plain_ankle).length()
    );
}

#[test]
fn missing_probe_and_missing_leg_are_reported() {
    let (skeleton, clip) = walk_clip();
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(DT)));
    let rig = spawn_rig(&mut app, skeleton.clone(), clip.clone(), Some(placement()));
    for _ in 0..8 {
        app.update();
    }
    let status = app.world().get::<FootPlacementStatus>(rig).expect("status");
    assert!(!status.probe_installed);
    assert!(status.legs_resolved);
    assert_eq!(report(&app, rig).weight, 0.0);

    let mut broken = placement();
    broken.legs[0].hip = "no-such-joint".into();
    let mut probed = app_with(SlopeProbe::new(Vec3::ZERO, 0.0, 0.0));
    let rig = spawn_rig(&mut probed, skeleton, clip, Some(broken));
    probed.update();
    let status = probed.world().get::<FootPlacementStatus>(rig).expect("status");
    assert!(status.probe_installed);
    assert!(!status.legs_resolved);
}

fn bind_at(x: f32, y: f32, z: f32) -> LocalBind {
    LocalBind {
        translation: Vec3::new(x, y, z),
        ..LocalBind::default()
    }
}

/// Hip, bent knee, ankle. Bind ankle height is 0.20 m and the chain can still reach the
/// ground, so a target that keeps the bind lift leaves the tip there.
fn bent_leg() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["hip".into(), "knee".into(), "ankle".into()],
        vec![None, Some(0), Some(1)],
        vec![
            bind_at(0.0, 0.90, 0.0),
            bind_at(0.0, -0.40, 0.35),
            bind_at(0.0, -0.30, -0.30),
        ],
    )
    .expect("bent leg")
}

fn tip_graph() -> AnimGraph {
    AnimGraph {
        parameters: Vec::new(),
        root: AnimNode::TwoBoneIK(TwoBoneIKNode {
            inner: Box::new(AnimNode::Clip(ClipNode {
                clip: "idle".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            })),
            chain: ["hip".into(), "knee".into(), "ankle".into()],
            target: IkTarget::Param("Foot0Target".into()),
            pole: Some("Foot0Pole".into()),
            weight_param: Some("Foot0Weight".into()),
        }),
    }
}

/// Graph output plants the tip joint on the contact. Pose output keeps the bind lift so the
/// sole meets the ground; a `TwoBoneIK` node moves the tip, and leaving the lift in sits the
/// joint a bind height above the probe.
#[test]
fn graph_params_plants_the_tip_on_the_contact() {
    let skeleton = Arc::new(bent_leg());
    let bind = skeleton.bind_pose_pose();
    let ankle = skeleton.joint_index("ankle").expect("ankle") as usize;
    let ankle_height = skeleton.globals(&bind, None)[ankle].w_axis.y;
    assert!(
        ankle_height > 0.15,
        "bind lift {ankle_height} must exceed the 2 cm contact band"
    );

    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("idle", 1.0));
    let animator = Animator::graph(tip_graph(), clips);
    let mut placement = FootPlacement::default();
    placement.legs = vec![FootLegDesc {
        hip: "hip".into(),
        knee: "knee".into(),
        ankle: "ankle".into(),
        toe: None,
        pole_offset: Vec3::ZERO,
    }];
    placement.output = FootPlacementOutput::GraphParams {
        target: "Foot{i}Target".into(),
        weight: "Foot{i}Weight".into(),
        pole: "Foot{i}Pole".into(),
    };

    let mut app = app_with(SlopeProbe::new(Vec3::ZERO, 0.0, 0.0));
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let rig = app
        .world_mut()
        .spawn((Transform::default(), binding, animator, placement))
        .id();
    // Blend-in is 5/s. The first frame has no pre-IK snapshot and does not plan.
    for _ in 0..30 {
        app.update();
    }
    let report = report(&app, rig);
    let foot = ankle_world(&app, rig, ankle);
    assert!(
        report.weight > 0.99 && report.planted.first().copied().unwrap_or(0.0) > 0.99,
        "weight {} planted {:?}",
        report.weight,
        report.planted
    );
    assert!(
        foot.y.abs() <= 0.02,
        "tip y {} m, bind height {ankle_height} (a kept lift sits on the bind height)",
        foot.y
    );
}

/// Rapier's solid ray reports a start inside a convex shape as a hit at the ray origin
/// (`toi = 0`, normal opposite the ray). `PhysicsGroundProbe` returns that hit unchanged.
struct StartsInside;

impl GroundProbe for StartsInside {
    fn probe(&self, from: Vec3, down: Vec3, _max_distance: f32, _ignore: Entity) -> Option<GroundHit> {
        let down = down.normalize_or_zero();
        let normal = if down == Vec3::ZERO { Vec3::Y } else { -down };
        Some(GroundHit { point: from, normal })
    }
}

/// Review proof. A probe that starts inside the ground must not plant the tip on the ray
/// origin (`probe_height` above the animated foot). The floor in this scene is `y = 0`.
#[test]
fn review_ray_that_starts_inside_does_not_plant_the_foot() {
    let skeleton = Arc::new(bent_leg());
    let ankle = skeleton.joint_index("ankle").expect("ankle") as usize;
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("idle", 1.0));
    let animator = Animator::graph(tip_graph(), clips);
    let mut placement = FootPlacement::default();
    placement.legs = vec![FootLegDesc {
        hip: "hip".into(),
        knee: "knee".into(),
        ankle: "ankle".into(),
        toe: None,
        pole_offset: Vec3::ZERO,
    }];
    placement.output = FootPlacementOutput::GraphParams {
        target: "Foot{i}Target".into(),
        weight: "Foot{i}Weight".into(),
        pole: "Foot{i}Pole".into(),
    };
    let mut app = app_with(StartsInside);
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let rig = app
        .world_mut()
        .spawn((Transform::default(), binding, animator, placement))
        .id();
    for _ in 0..30 {
        app.update();
    }
    let foot = ankle_world(&app, rig, ankle);
    assert!(
        foot.y.abs() <= 0.05,
        "interior hit planted the tip at y {} (probe origin is probe_height above the foot)",
        foot.y
    );
}

/// A flat floor at y = 0 that reports a shared ground velocity (a platform the test moves).
struct MovingFloor(Arc<std::sync::Mutex<Vec3>>);

impl GroundProbe for MovingFloor {
    fn probe(&self, from: Vec3, _down: Vec3, max_distance: f32, _ignore: Entity) -> Option<GroundHit> {
        (from.y >= 0.0 && from.y <= max_distance).then_some(GroundHit {
            point: Vec3::new(from.x, 0.0, from.z),
            normal: Vec3::Y,
        })
    }

    fn probe_moving(
        &self,
        from: Vec3,
        down: Vec3,
        max_distance: f32,
        ignore: Entity,
    ) -> Option<(GroundHit, Vec3)> {
        let velocity = *self.0.lock().unwrap_or_else(|p| p.into_inner());
        self.probe(from, down, max_distance, ignore)
            .map(|hit| (hit, velocity))
    }
}

/// Rig x minus planted ankle x after the body rode `frames` frames at 1 m/s; `reported` is the
/// ground velocity the probe reports while it moves.
fn platform_lag(reported: Vec3, frames: usize) -> (f32, FootPlantReport) {
    let skeleton = Arc::new(bent_leg());
    let ankle = skeleton.joint_index("ankle").expect("ankle") as usize;
    let mut placement = FootPlacement::default();
    placement.legs = vec![FootLegDesc {
        hip: "hip".into(),
        knee: "knee".into(),
        ankle: "ankle".into(),
        toe: None,
        pole_offset: Vec3::ZERO,
    }];
    let velocity = Arc::new(std::sync::Mutex::new(Vec3::ZERO));
    let mut app = app_with(MovingFloor(velocity.clone()));
    let rig = spawn_rig(
        &mut app,
        skeleton,
        AnimationClip::empty("idle", 1.0),
        Some(placement),
    );
    for _ in 0..30 {
        app.update();
    }
    let settled = report(&app, rig);
    assert!(
        settled.weight > 0.99 && settled.lock.first().copied() == Some(1.0),
        "{settled:?}"
    );
    let offset = ankle_world(&app, rig, ankle).x;
    *velocity.lock().unwrap_or_else(|p| p.into_inner()) = reported;
    let mut x = 0.0;
    for _ in 0..frames {
        x += 1.0 * DT;
        set_transform(&mut app, rig, Vec3::new(x, 0.0, 0.0), Quat::IDENTITY);
        app.update();
    }
    let lag = x + offset - ankle_world(&app, rig, ankle).x;
    (lag, report(&app, rig))
}

/// A platform carries the whole body at 1 m/s (under the 1.5 m/s stance limit). With the
/// ground velocity from `GroundProbe::probe_moving`, the planted foot rides with it (ADR-8721).
/// The same body moved over still ground (velocity 0: an idle clip while the capsule moves)
/// keeps the world pin, so the foot falls behind: the velocity is what tells them apart.
#[test]
fn a_moving_platform_carries_the_planted_foot() {
    let (lag, carried) = platform_lag(Vec3::X, 12);
    assert!(lag.abs() < 0.01, "foot {lag} m behind the platform after 0.2 m");
    assert_eq!(carried.lock.first().copied(), Some(1.0), "{carried:?}");
    assert!(carried.max_slide < 0.01, "{carried:?}");

    let (lag, pinned) = platform_lag(Vec3::ZERO, 12);
    assert!(lag > 0.15, "still ground must keep the world pin: lag {lag}");
    assert_eq!(pinned.lock.first().copied(), Some(1.0), "{pinned:?}");
}

/// The angle in degrees of the rotation that takes `from` onto `to`.
fn angle_deg(from: Quat, to: Quat) -> f32 {
    let delta = (to * from.inverse()).normalize();
    (2.0 * delta.w.abs().clamp(0.0, 1.0).acos()).to_degrees()
}

/// L-05 item 4 (ADR-10130): graph output turns the foot by the planner's tilt. On a 15 degree
/// slope the ankle joint's object rotation is the knee's (the clip keeps the ankle's bind
/// rotation, identity here) turned by the reported tilt; before the fix the graph's `TwoBoneIK`
/// left the foot following the shin, the tilt only reported, never applied.
#[test]
fn graph_params_turns_the_foot_by_the_tilt() {
    let skeleton = Arc::new(bent_leg());
    let knee = skeleton.joint_index("knee").expect("knee") as usize;
    let ankle = skeleton.joint_index("ankle").expect("ankle") as usize;

    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("idle", 1.0));
    let animator = Animator::graph(tip_graph(), clips);
    let mut placement = FootPlacement::default();
    placement.legs = vec![FootLegDesc {
        hip: "hip".into(),
        knee: "knee".into(),
        ankle: "ankle".into(),
        toe: None,
        pole_offset: Vec3::ZERO,
    }];
    placement.output = FootPlacementOutput::GraphParams {
        target: "Foot{i}Target".into(),
        weight: "Foot{i}Weight".into(),
        pole: "Foot{i}Pole".into(),
    };

    let mut app = app_with(SlopeProbe::new(Vec3::ZERO, 0.0, 15.0));
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let rig = app
        .world_mut()
        .spawn((Transform::default(), binding, animator, placement))
        .id();
    for _ in 0..60 {
        app.update();
    }
    let report = report(&app, rig);
    let tilt = report.tilt_deg.first().copied().unwrap_or(0.0);
    assert!(
        report.weight > 0.99 && tilt > 5.0,
        "the planner tilts the foot on a 15 degree slope: weight {} tilt {tilt}",
        report.weight
    );
    let globals = &app.world().get::<JointGlobals>(rig).expect("globals").globals;
    let rotation = |joint: usize| globals[joint].to_scale_rotation_translation().1;
    let turned = angle_deg(rotation(knee), rotation(ankle));
    assert!(
        (turned - tilt).abs() <= 0.5,
        "the ankle is turned {turned} degrees from the shin; the planner's tilt is {tilt}"
    );
}

/// Graph output on level ground keeps the animator's pose without a copy: no tilt and no toe
/// bend, so the solved foot follows the shin exactly as before ADR-10130.
#[test]
fn graph_params_on_level_ground_leaves_the_foot_on_the_shin() {
    let skeleton = Arc::new(bent_leg());
    let knee = skeleton.joint_index("knee").expect("knee") as usize;
    let ankle = skeleton.joint_index("ankle").expect("ankle") as usize;
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("idle", 1.0));
    let animator = Animator::graph(tip_graph(), clips);
    let mut placement = FootPlacement::default();
    placement.legs = vec![FootLegDesc {
        hip: "hip".into(),
        knee: "knee".into(),
        ankle: "ankle".into(),
        toe: None,
        pole_offset: Vec3::ZERO,
    }];
    placement.output = FootPlacementOutput::GraphParams {
        target: "Foot{i}Target".into(),
        weight: "Foot{i}Weight".into(),
        pole: "Foot{i}Pole".into(),
    };
    let mut app = app_with(SlopeProbe::new(Vec3::ZERO, 0.0, 0.0));
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let rig = app
        .world_mut()
        .spawn((Transform::default(), binding, animator, placement))
        .id();
    for _ in 0..60 {
        app.update();
    }
    let globals = &app.world().get::<JointGlobals>(rig).expect("globals").globals;
    let rotation = |joint: usize| globals[joint].to_scale_rotation_translation().1;
    let turned = angle_deg(rotation(knee), rotation(ankle));
    assert!(turned <= 1.0e-3, "a level foot is not turned: {turned} degrees");
}
