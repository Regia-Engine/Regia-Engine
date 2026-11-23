// SPDX-License-Identifier: MPL-2.0
//! Montage extensions (`docs/specs/34-montage-extensions.md`, ADR-1644): checked play and the
//! terminal-event contract (M5), root-motion routes (M6) and scripted warps through the plugin
//! (M3). Every input is fixed (frame times, targets, clips); no wall clock.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::clip::{AnimationClip, JointTrack, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::montage::{RateCurve, RateInterpolation};
use regia_animation::montage_player::{MontageFrame, MontageRecord, RigPlacement};
use regia_animation::pose::translated;
use regia_animation::{
    AnimationPlugin, Animator, ClipLibrary, Montage, MontageEventKind, MontagePlayer, PlayError, PlayOptions,
    RootMotionOutput, RootMotionRoute, ScriptedWarp, ScriptedWarpEvent, ScriptedWarps, Skeleton, SlotDef,
    WarpEvent, WarpTarget,
};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_scene::Transform;

fn skeleton() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["root".into(), "arm".into()],
        vec![None, Some(0)],
        vec![translated(Vec3::ZERO), translated(Vec3::Y)],
    )
    .expect("valid skeleton")
}

/// A clip whose root walks 1 m/s toward −Z with XZ root motion.
fn walk(name: &str, duration: f32) -> AnimationClip {
    let mut clip = AnimationClip::empty(name, duration);
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

fn library() -> ClipLibrary {
    let mut clips = ClipLibrary::new();
    clips.insert(walk("strike", 2.0));
    clips
}

fn player() -> MontagePlayer {
    MontagePlayer::new(vec![SlotDef::full_body("full_body", "body")], library())
}

fn montage(name: &str) -> Montage {
    let mut m = Montage::single(name, "full_body", "strike");
    m.blend_in.time = 0.0;
    m.blend_out.time = 0.0;
    m
}

fn kinds(events: &[MontageRecord], id: regia_animation::MontageId) -> Vec<&'static str> {
    events
        .iter()
        .filter(|e| e.id == id)
        .map(|e| match &e.kind {
            MontageEventKind::Started => "started",
            MontageEventKind::BlendingOut { interrupted: true } => "blend_out true",
            MontageEventKind::BlendingOut { interrupted: false } => "blend_out false",
            MontageEventKind::Ended { interrupted: true } => "ended true",
            MontageEventKind::Ended { interrupted: false } => "ended false",
            _ => "other",
        })
        .collect()
}

/// One player frame over a bind-pose graph.
fn step(player: &mut MontagePlayer, skeleton: &Skeleton, dt: f32) -> Vec<MontageRecord> {
    let mut pose = skeleton.bind_pose_pose();
    let mut root = (Vec3::ZERO, Quat::IDENTITY);
    let frame = MontageFrame {
        skeleton,
        graph_clips: None,
        delta: dt,
        rig: RigPlacement::default(),
    };
    player.evaluate(&frame, &mut pose, &mut root, &mut Vec::new());
    player.take_events()
}

#[test]
fn try_play_names_each_kind_of_bad_request_and_starts_nothing() {
    let mut p = player();
    let ok = Arc::new(montage("ok"));
    let mut unknown_slot = montage("odd");
    unknown_slot.tracks[0].slot = "tail".into();
    assert_eq!(
        p.try_play(Arc::new(unknown_slot), PlayOptions::default()),
        Err(PlayError::UnknownSlot("tail".into()))
    );
    let bad_section = PlayOptions {
        section: Some("nope".into()),
        ..PlayOptions::default()
    };
    assert_eq!(
        p.try_play(ok.clone(), bad_section),
        Err(PlayError::UnknownSection("nope".into()))
    );
    for rate in [-1.0, f32::NAN, f32::INFINITY] {
        let options = PlayOptions {
            rate,
            ..PlayOptions::default()
        };
        assert_eq!(p.try_play(ok.clone(), options), Err(PlayError::BadRate), "{rate}");
    }
    for start_time in [-0.5, f32::NAN, 99.0] {
        let options = PlayOptions {
            start_time,
            ..PlayOptions::default()
        };
        assert_eq!(
            p.try_play(ok.clone(), options),
            Err(PlayError::StartPastEnd),
            "{start_time}"
        );
    }
    let mut empty = montage("empty");
    empty.tracks[0].segments.clear();
    assert_eq!(
        p.try_play(Arc::new(empty), PlayOptions::default()),
        Err(PlayError::NoMainTrack)
    );
    assert!(!p.is_active(), "a refused request starts nothing");
    assert!(p.take_events().is_empty());
    let id = p.try_play(ok, PlayOptions::default()).expect("valid request");
    assert_eq!(p.instances().len(), 1);
    assert_eq!(p.instances()[0].id, id);
}

#[test]
fn a_completed_montage_reports_uninterrupted_events_only() {
    let skeleton = skeleton();
    let mut p = player();
    let mut m = montage("short");
    m.tracks[0].segments[0].end = Some(0.5);
    let id = p.play(Arc::new(m), PlayOptions::default());
    let mut events = Vec::new();
    for _ in 0..20 {
        events.extend(step(&mut p, &skeleton, 0.1));
    }
    assert_eq!(kinds(&events, id), ["started", "blend_out false", "ended false"]);
}

#[test]
fn a_queued_montage_that_is_stopped_gets_one_ended_and_no_started() {
    let skeleton = skeleton();
    let mut p = player();
    let first = p.play(Arc::new(montage("first")), PlayOptions::default());
    let queued = p.queue(Arc::new(montage("second")), PlayOptions::default());
    assert_eq!(p.queued(), vec![queued]);
    let mut events = step(&mut p, &skeleton, 0.1);
    assert_eq!(p.stop(None, Some(0.0)), 1);
    events.extend(p.take_events());
    events.extend(step(&mut p, &skeleton, 0.1));
    assert!(p.queued().is_empty());
    assert_eq!(kinds(&events, queued), ["ended true"]);
    assert_eq!(kinds(&events, first), ["started", "blend_out true", "ended true"]);
}

#[test]
fn stopping_one_queued_montage_leaves_the_others_queued() {
    let mut p = player();
    p.play(Arc::new(montage("first")), PlayOptions::default());
    let a = p.queue(Arc::new(montage("a")), PlayOptions::default());
    let b = p.queue(Arc::new(montage("b")), PlayOptions::default());
    assert_eq!(p.stop(Some(a), None), 0, "no instance was blending");
    assert_eq!(p.queued(), vec![b]);
    let events = p.take_events();
    assert_eq!(kinds(&events, a), ["ended true"]);
    assert!(kinds(&events, b).is_empty());
}

#[test]
fn cancel_all_ends_everything_once_with_interrupted_events() {
    let skeleton = skeleton();
    let mut p = player();
    let playing = p.play(Arc::new(montage("playing")), PlayOptions::default());
    let queued = p.queue(Arc::new(montage("queued")), PlayOptions::default());
    let mut events = step(&mut p, &skeleton, 0.1);
    p.cancel_all();
    events.extend(p.take_events());
    assert!(!p.is_active());
    assert_eq!(
        kinds(&events, playing),
        ["started", "blend_out true", "ended true"]
    );
    assert_eq!(kinds(&events, queued), ["ended true"]);
    // A second call has nothing left to say.
    p.cancel_all();
    assert!(p.take_events().is_empty());
}

#[test]
fn a_hold_without_auto_blend_out_never_ends_until_stopped() {
    let skeleton = skeleton();
    let mut p = player();
    let mut m = montage("pose");
    m.auto_blend_out = false;
    m.tracks[0].segments[0].end = Some(0.3);
    let id = p.play(Arc::new(m), PlayOptions::default());
    let mut events = Vec::new();
    for _ in 0..30 {
        events.extend(step(&mut p, &skeleton, 0.1));
    }
    assert_eq!(
        kinds(&events, id),
        ["started"],
        "no blend-out and no end while holding"
    );
    p.stop(Some(id), Some(0.0));
    events.extend(step(&mut p, &skeleton, 0.1));
    assert_eq!(kinds(&events, id), ["started", "blend_out true", "ended true"]);
}

/// A plugin app with one rig walking 1 m/s toward −Z; `apply` moves the rig by its root motion.
fn walking_app(dt: f32) -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_secs_f32(dt),
    ));
    app.add_systems(
        Update,
        apply_root_motion.after(regia_animation::systems::evaluate_animators),
    );
    let skeleton = Arc::new(skeleton());
    let rig = app.world_mut().spawn(Transform::default()).id();
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    app.world_mut()
        .entity_mut(rig)
        .insert((binding, Animator::single(walk("walk", 4.0), true)));
    (app, rig)
}

fn apply_root_motion(mut rigs: Query<(&mut Transform, &mut RootMotionOutput)>) {
    for (mut transform, mut output) in &mut rigs {
        let (translation, rotation, _) = output.take();
        let step = transform.rotation * (transform.scale * translation);
        transform.translation += step;
        transform.rotation = (transform.rotation * rotation).normalize();
    }
}

/// Runs `frames` updates and returns the rig's distance walked toward −Z and the last
/// root-motion delta the animator reported.
fn walked(route: RootMotionRoute, frames: usize) -> (f32, Vec3) {
    let dt = 1.0 / 32.0;
    let (mut app, rig) = walking_app(dt);
    app.world_mut()
        .entity_mut(rig)
        .get_mut::<Animator>()
        .expect("animator")
        .root_motion_route = route;
    for _ in 0..frames {
        app.update();
    }
    let world = app.world();
    let moved = -world
        .entity(rig)
        .get::<Transform>()
        .expect("transform")
        .translation
        .z;
    let last = world
        .entity(rig)
        .get::<Animator>()
        .expect("animator")
        .last_root_motion()
        .0;
    (moved, last)
}

#[test]
fn root_motion_routes_decide_what_moves_the_rig() {
    let frames = 33; // 32 frames of 1/32 s walk 1 m (the first update may carry no time)
    let (all, all_last) = walked(RootMotionRoute::GraphAndMontages, frames);
    assert!(
        (0.9..1.1).contains(&all),
        "default route walks about 1 m, got {all}"
    );
    assert!(all_last.z < 0.0);

    let (report, report_last) = walked(RootMotionRoute::Report, frames);
    assert!(report.abs() < 1e-6, "report applies nothing, moved {report}");
    assert!(report_last.z < 0.0, "report still exposes the motion");

    let (ignore, ignore_last) = walked(RootMotionRoute::Ignore, frames);
    assert!(ignore.abs() < 1e-6, "ignore applies nothing, moved {ignore}");
    assert_eq!(ignore_last, Vec3::ZERO, "ignore discards the motion");

    let (only, only_last) = walked(RootMotionRoute::OnlyMontages, frames);
    assert!(
        only.abs() < 1e-6,
        "no montage plays, so graph motion is dropped: {only}"
    );
    assert_eq!(only_last, Vec3::ZERO);
}

#[test]
fn a_scripted_warp_through_the_plugin_lands_the_rig_on_its_target() {
    let dt = 1.0 / 32.0;
    let (mut app, rig) = walking_app(dt);
    let target = Vec3::new(0.0, 0.0, -3.0);
    let mut warps = ScriptedWarps::new();
    warps.set_target("enemy", WarpTarget::at(target));
    warps
        .add(ScriptedWarp::new("dash", "enemy", 1.0))
        .expect("valid warp");
    app.world_mut().entity_mut(rig).insert(warps);
    let mut cursor = MessageCursor::<ScriptedWarpEvent>::default();
    let mut landed = None;
    for _ in 0..60 {
        app.update();
        let finished = cursor
            .read(app.world().resource::<Messages<ScriptedWarpEvent>>())
            .any(|e| e.entity == rig && e.event == WarpEvent::Finished { id: "dash".into() });
        if finished && landed.is_none() {
            landed = app.world().entity(rig).get::<Transform>().map(|t| t.translation);
        }
    }
    let landed = landed.expect("the warp finishes");
    assert!((landed - target).length() < 0.01, "ended at {landed}");
    let warps = app
        .world()
        .entity(rig)
        .get::<ScriptedWarps>()
        .expect("component stays");
    assert!(!warps.is_active());
}

#[test]
fn a_rate_curve_changes_how_fast_montage_time_runs() {
    let skeleton = skeleton();
    let mut p = player();
    let mut m = montage("curved");
    // Twice as fast for the first montage second, then half speed.
    m.rate_curve = Some(RateCurve {
        keys: vec![(0.0, 2.0), (1.0, 0.5)],
        interpolation: RateInterpolation::Step,
    });
    m.validate().expect("valid curve");
    let id = p.play(Arc::new(m), PlayOptions::default());
    // 0.5 wall seconds cover 1.0 montage seconds.
    for _ in 0..5 {
        step(&mut p, &skeleton, 0.1);
    }
    let time = p.status(id).expect("still playing").time;
    assert!((time - 1.0).abs() < 1e-4, "{time}");
    // 0.2 more wall seconds at half speed cover 0.1.
    for _ in 0..2 {
        step(&mut p, &skeleton, 0.1);
    }
    let time = p.status(id).expect("still playing").time;
    assert!((time - 1.1).abs() < 1e-4, "{time}");
}

#[test]
fn a_bad_rate_curve_is_refused_by_validation() {
    for keys in [
        vec![],
        vec![(1.0, 1.0), (1.0, 2.0)],
        vec![(0.0, 0.0)],
        vec![(0.0, f32::NAN)],
        vec![(-1.0, 1.0)],
    ] {
        let mut m = montage("bad");
        m.rate_curve = Some(RateCurve {
            keys,
            interpolation: RateInterpolation::Linear,
        });
        assert!(m.validate().is_err());
    }
}
