// SPDX-License-Identifier: MPL-2.0
//! Cooked foot-contact curves (ADR-8308, `docs/specs/17-animation.md` § 6): the curves blend through
//! a blend space like any named curve, and the planter's contact weight from a curve plants a clip
//! whose sole is off the rig origin, where the height rule cannot.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use regia_animation::blendspace::{BlendSample1D, BlendSpace1D};
use regia_animation::clip::{AnimationClip, NamedCurve, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::demo_rig::{
    DEMO_LEGS, DemoGait, DemoRig, demo_humanoid, demo_in_place, demo_locomotion,
};
use regia_animation::foot_ik::contact::{
    ContactCurveSettings, add_contact_curves, contact_curve, contact_curve_name, contact_frame_times,
    contact_weight, label_clip_contacts,
};
use regia_animation::foot_ik::{FootPlantInput, FootPlantLeg, FootPlantSettings, FootPlanter};
use regia_animation::graph::{AnimGraph, AnimNode, BlendSpace1DNode};
use regia_animation::ik::joint_position;
use regia_animation::params::{ParamDecl, ParamValue};
use regia_animation::prelude::*;
use regia_animation::skeleton::LocalBind;
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;

/// A one-second clip with no tracks whose `contact.foot_l` curve is planted on frames
/// `from..=to` of 31.
fn labeled(name: &str, from: usize, to: usize) -> AnimationClip {
    let mut clip = AnimationClip::empty(name, 1.0);
    let times = contact_frame_times(&clip);
    let labels: Vec<bool> = (0..times.len()).map(|i| (from..=to).contains(&i)).collect();
    clip.curves.push(NamedCurve {
        name: contact_curve_name("foot_l"),
        curve: contact_curve(&times, &labels),
    });
    clip
}

/// Two clips with offset contacts (A planted on frames 0..=18, B on 12..=30) mixed 50/50 by a 1D
/// blend space: the graph's `contact.foot_l` reads 1 where both are planted and 0.5 where one is.
#[test]
fn blend_space_mix_blends_contact_weights() {
    let mut clips = ClipLibrary::new();
    clips.insert(labeled("a", 0, 18));
    clips.insert(labeled("b", 12, 30));
    let graph = AnimGraph {
        parameters: vec![ParamDecl("Speed".into(), ParamValue::Float(0.5))],
        root: AnimNode::BlendSpace1D(BlendSpace1DNode {
            space: BlendSpace1D {
                param: "Speed".into(),
                samples: [(0.0, "a"), (1.0, "b")]
                    .iter()
                    .map(|&(value, clip)| BlendSample1D {
                        value,
                        clip: clip.into(),
                    })
                    .collect(),
            },
            sync_group: None,
        }),
    };
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    // 0.1 s steps: clip times 0.1, 0.2, ... (frames 3, 6, ...).
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_millis(100),
    ));
    app.update();
    let skeleton = Skeleton::from_local_bind(vec!["root".into()], vec![None], vec![LocalBind::default()])
        .expect("skeleton");
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton));
    // `Animator::graph` starts from an empty parameter set. The declaration default is not
    // copied, so the 50/50 mix has to be set on the animator or the space stays on sample A.
    let mut animator = Animator::graph(graph, clips);
    animator.set("Speed", 0.5);
    let rig = app
        .world_mut()
        .spawn((Transform::default(), binding, animator))
        .id();
    let mut seen = Vec::new();
    for _ in 0..9 {
        app.update();
        let animator = app.world().get::<Animator>(rig).expect("animator");
        seen.push(contact_weight(&animator.parameters, "foot_l").expect("the graph wrote the curve"));
    }
    // Frames 3..27 in steps of 3 (time = step * 0.1 after the warm-up frame at 0).
    let both = seen.iter().filter(|&&w| (w - 1.0).abs() < 1e-5).count();
    let half = seen.iter().filter(|&&w| (w - 0.5).abs() < 1e-5).count();
    assert!(
        both >= 1 && half >= 4 && both + half == seen.len(),
        "contact weights {seen:?}: expected 1 where both clips plant and 0.5 where one does"
    );
    // The same weight as a planter input: half planted, never pinned.
    let mut planter = FootPlanter::new(FootPlantSettings::default());
    let mut leg = FootPlantLeg::new(Vec3::new(0.1, 0.9, 0.0), Vec3::new(0.1, 0.1, 0.0), 0.8, 0.1);
    leg.contact = Some(0.5);
    for _ in 0..30 {
        let plan = planter.plan(&FootPlantInput {
            rig_translation: Vec3::ZERO,
            rig_rotation: Quat::IDENTITY,
            legs: std::slice::from_ref(&leg),
            active: true,
            dt: 1.0 / 60.0,
        });
        assert_eq!(plan.legs[0].planted, 0.5);
        assert_eq!(plan.legs[0].lock, 0.0);
    }
}

/// The demo walk as an in-place clip lifted 5 cm (its soles are 5 cm above the rig origin), played
/// while the character moves 10 % faster than the clip's feet slide back.
///
/// The planter's fallback rule (clip foot height over the bind ankle height) reads the stance foot
/// as 5 cm lifted: it is never fully planted, so it is never pinned (asserted), and the target
/// slides with the clip. The cooked contact curve (heights over the foot's own lowest point,
/// speeds over the ground the treadmill clip walks on) plants it on its stance frames, and the
/// pinned target slides at most 1 cm.
#[test]
fn contact_curve_beats_the_height_rule() {
    const DT: f32 = 1.0 / 60.0;
    const LIFT: f32 = 0.05;
    let skeleton = demo_humanoid(&DemoRig::default()).expect("demo rig");
    let gait = DemoGait::standard()
        .into_iter()
        .find(|g| g.name == "walk")
        .expect("walk gait");
    let mut clip = demo_in_place(
        &demo_locomotion(&skeleton, std::slice::from_ref(&gait))
            .pop()
            .expect("walk clip"),
    );
    let root = clip.tracks.iter_mut().find(|t| t.joint == 0).expect("root track");
    root.translations = match &root.translations {
        TrackCurve::Static(v) => TrackCurve::Static(*v + Vec3::Y * LIFT),
        TrackCurve::Animated {
            times,
            values,
            exact_rotation,
        } => TrackCurve::Animated {
            times: times.clone(),
            values: values.iter().map(|v| *v + Vec3::Y * LIFT).collect(),
            exact_rotation: *exact_rotation,
        },
    };
    let feet: Vec<String> = DEMO_LEGS
        .iter()
        .map(|leg| skeleton.joint_names[leg[2]].clone())
        .collect();
    let settings = ContactCurveSettings {
        feet: feet.clone(),
        treadmill: true,
        ..ContactCurveSettings::default()
    };
    assert_eq!(add_contact_curves(&mut clip, &skeleton, &settings), Ok(2));
    let labels = label_clip_contacts(&clip, &skeleton, &settings).expect("labels");
    for foot in &labels {
        let planted = foot.labels.iter().filter(|&&l| l).count();
        assert!(
            planted > foot.labels.len() / 4 && planted < foot.labels.len(),
            "{}: {planted} of {} frames planted",
            foot.name,
            foot.labels.len()
        );
    }

    let bind = skeleton.bind_pose_pose();
    let ankle_height = joint_position(&skeleton, &bind, DEMO_LEGS[0][2] as u32).y;
    let length: Vec<f32> = DEMO_LEGS
        .iter()
        .map(|[hip, knee, ankle]| {
            let h = joint_position(&skeleton, &bind, *hip as u32);
            let k = joint_position(&skeleton, &bind, *knee as u32);
            let a = joint_position(&skeleton, &bind, *ankle as u32);
            (k - h).length() + (a - k).length()
        })
        .collect();

    // Runs the walk; `curve` picks the contact source. Returns per frame and leg: (curve weight,
    // planted, lock, world target) after the first cycle.
    let run = |curve: bool| {
        let mut planter = FootPlanter::new(FootPlantSettings::default());
        let mut frames = Vec::new();
        let mut pose = bind.clone();
        let mut params = ParameterSet::default();
        let steps = (3.0 * gait.cycle / DT) as usize;
        for step in 0..steps {
            let t = step as f32 * DT;
            let rig = Vec3::NEG_Z * (gait.speed * 1.1 * t);
            let local = t % clip.duration;
            pose.clone_from(&bind);
            clip.sample_pose(local, &mut pose);
            for named in &clip.curves {
                params.set_float(&named.name, named.curve.sample(local));
            }
            let legs: Vec<FootPlantLeg> = DEMO_LEGS
                .iter()
                .zip(&length)
                .zip(&feet)
                .map(|((&[hip, _, ankle], &length), name)| {
                    let mut leg = FootPlantLeg::new(
                        rig + joint_position(&skeleton, &pose, hip as u32),
                        rig + joint_position(&skeleton, &pose, ankle as u32),
                        length,
                        ankle_height,
                    );
                    if curve {
                        leg.contact = contact_weight(&params, name);
                    }
                    leg
                })
                .collect();
            let plan = planter.plan(&FootPlantInput {
                rig_translation: rig,
                rig_rotation: Quat::IDENTITY,
                legs: &legs,
                active: true,
                dt: DT,
            });
            let report = planter.report(&plan);
            if t >= gait.cycle {
                frames.push(
                    (0..2)
                        .map(|i| {
                            let weight = feet.get(i).and_then(|n| contact_weight(&params, n));
                            (
                                weight,
                                plan.legs[i].planted,
                                plan.legs[i].lock,
                                rig + plan.legs[i].target,
                            )
                        })
                        .collect::<Vec<_>>(),
                );
                if curve {
                    assert!(report.max_slide <= 0.01, "t {t}: max_slide {}", report.max_slide);
                }
            }
        }
        frames
    };

    // The height rule: the stance foot reads 5 cm lifted and is never pinned.
    let height = run(false);
    assert!(
        height
            .iter()
            .flatten()
            .all(|&(_, planted, lock, _)| planted < 0.999 && lock == 0.0),
        "the height rule pinned a foot whose sole is 5 cm above the origin"
    );

    // The curve: stance frames pin, and a pinned target stays within 1 cm of its touchdown.
    let curve = run(true);
    let mut pinned_frames = 0;
    for leg in 0..2 {
        let mut start: Option<Vec3> = None;
        let mut worst = 0.0_f32;
        for frame in &curve {
            let (weight, _, lock, target) = frame[leg];
            if weight == Some(1.0) && lock == 1.0 {
                pinned_frames += 1;
                let from = *start.get_or_insert(target);
                worst = worst.max(Vec3::new(target.x - from.x, 0.0, target.z - from.z).length());
            } else {
                start = None;
            }
        }
        assert!(worst <= 0.01, "leg {leg}: a pinned target slid {worst} m");
    }
    assert!(
        pinned_frames > curve.len() / 2,
        "only {pinned_frames} pinned leg-frames"
    );
}
