// SPDX-License-Identifier: MPL-2.0
//! Spec 34 items deferred by the 11-10 audit (sprint post05-anim-abilities, ADR-7216):
//!
//! - § 9: every runtime change to a player is a [`MontageCommand`]; a recorded stream replays on a
//!   second app and gives the same instances and events.
//! - M1: a montage with an unknown slot warns when a player loads it and fails the cook check.
//! - M4: a `rescale: false` variant reports the authored items that now lie past its end.
//! - M5: despawning the owner of a player cuts its montages with interrupted events.
//!
//! No wall clock: every input is fixed.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::clip::{AnimationClip, JointTrack, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::montage::{MontageNotify, MontageSection, SectionNext, WarpWindow};
use regia_animation::montage_command::{
    ApplyMontageCommand, MontageCommand, MontageCommandRecorded, TimedMontageCommand,
};
use regia_animation::pose::translated;
use regia_animation::skeleton_slots::{SkeletonSlots, cook_slot_errors};
use regia_animation::{
    AnimationPlugin, Animator, ClipLibrary, Montage, MontageEvent, MontageEventKind, MontageId,
    MontageLibrary, MontagePlayer, MontageStatus, MontageVariant, PlayOptions, SegmentOverride, Skeleton,
    SlotDef, WarpTarget,
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

/// A clip of `duration` seconds: the root walks 1 m/s toward −Z, the arm rises.
fn strike(name: &str, duration: f32) -> AnimationClip {
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
    clip.tracks.push(JointTrack {
        joint: 1,
        translations: TrackCurve::Animated {
            times: vec![0.0, duration],
            values: vec![Vec3::Y, Vec3::new(0.0, 3.0, 0.0)],
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
    clips.insert(strike("strike", 2.0));
    clips.insert(strike("short", 0.5));
    clips
}

/// Sections `windup` 0–0.5, `loop` 0.5–1.0 (loops itself), `finish` 1.0–2.0 (stop); a warp
/// window toward `enemy` and a notify.
fn combo() -> Montage {
    let mut m = Montage::single("combo", "full_body", "strike");
    m.blend_in.time = 0.1;
    m.blend_out.time = 0.2;
    m.sections = vec![
        MontageSection {
            follow: None,
            name: "windup".into(),
            start: 0.0,
            next: SectionNext::Continue,
        },
        MontageSection {
            follow: None,
            name: "loop".into(),
            start: 0.5,
            next: SectionNext::Jump("loop".into()),
        },
        MontageSection {
            follow: None,
            name: "finish".into(),
            start: 1.0,
            next: SectionNext::Stop,
        },
    ];
    m.notifies = vec![MontageNotify {
        follow: None,
        name: "hit".into(),
        time: 1.25,
        duration: 0.0,
        payload: None,
        branch: false,
    }];
    m.warps = vec![WarpWindow {
        target: "enemy".into(),
        start: 0.2,
        end: 0.9,
        translation: true,
        vertical: false,
        rotation: true,
        ..WarpWindow::new("", 0.0, 0.0)
    }];
    m
}

fn player() -> MontagePlayer {
    let mut player = MontagePlayer::new(
        vec![
            SlotDef::full_body("full_body", "body"),
            SlotDef::masked("upper", "upper", &["arm"]),
        ],
        library(),
    );
    player.add_montage(combo());
    let mut wave = Montage::single("wave", "upper", "short");
    wave.blend_in.time = 0.05;
    player.add_montage(wave);
    player
}

const DT: f32 = 1.0 / 30.0;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_secs_f32(DT),
    ));
    app
}

fn spawn_rig(app: &mut App, player: MontagePlayer) -> Entity {
    let rig = app.world_mut().spawn(Transform::default()).id();
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton()));
    let idle = AnimationClip::empty("idle", 1.0);
    app.world_mut()
        .entity_mut(rig)
        .insert((binding, Animator::single(idle, true), player));
    rig
}

fn kind(kind: &MontageEventKind) -> String {
    match kind {
        MontageEventKind::Started => "started".into(),
        MontageEventKind::Section { from, to } => format!("section {from}->{to}"),
        MontageEventKind::Notify { name, .. } => format!("notify {name}"),
        MontageEventKind::WindowBegin { name, .. } => format!("begin {name}"),
        MontageEventKind::WindowEnd { name } => format!("end {name}"),
        MontageEventKind::BranchPoint { name } => format!("branch {name}"),
        MontageEventKind::BlendingOut { interrupted } => format!("blend_out {interrupted}"),
        MontageEventKind::Ended { interrupted } => format!("ended {interrupted}"),
    }
}

/// `(frame, id, montage, kind)` of every montage message `rig` got this update.
fn drain_events(
    app: &App,
    cursor: &mut MessageCursor<MontageEvent>,
    rig: Entity,
    frame: u32,
    out: &mut Vec<(u32, u64, String, String)>,
) {
    for event in cursor.read(app.world().resource::<Messages<MontageEvent>>()) {
        assert_eq!(event.entity, rig);
        out.push((frame, event.id.0, event.montage.clone(), kind(&event.kind)));
    }
}

const FRAMES: u32 = 90;

/// The gameplay script of the recording app: calls on the player between updates.
fn script(frame: u32, player: &mut MontagePlayer) {
    match frame {
        2 => {
            player.set_warp_target("enemy", WarpTarget::at(Vec3::new(1.0, 0.0, -2.0)));
            let _ = player.play_named("combo", PlayOptions::default());
        }
        12 => player.set_warp_target("enemy", WarpTarget::at(Vec3::new(1.5, 0.0, -2.5))),
        20 => {
            let _ = player.play_named("wave", PlayOptions::default());
        }
        28 => {
            // Ids are allocated in call order: the first combo is 1.
            let _ = player.set_next_section(Some(MontageId(1)), "loop", SectionNext::Continue);
        }
        33 => {
            let _ = player.set_rate(None, 1.5);
        }
        45 => {
            let _ = player.jump_to_section(None, "finish");
        }
        55 => {
            let _ = player.play_named(
                "combo",
                PlayOptions {
                    section: Some("loop".into()),
                    ..PlayOptions::default()
                },
            );
        }
        70 => {
            let _ = player.stop(None, Some(0.1));
        }
        _ => {}
    }
}

/// § 9: a recorded command stream, sent as bytes, replays on a fresh app: the same montage
/// messages on the same frames and the same instance snapshots every frame.
#[test]
fn a_recorded_montage_play_replays_on_a_second_app() {
    // The recording app: gameplay calls the player; the player records each call as a command.
    let mut a = app();
    let mut recording = player();
    recording.set_recording(true);
    let rig_a = spawn_rig(&mut a, recording);
    let mut events_a = Vec::new();
    let mut statuses_a: Vec<Vec<MontageStatus>> = Vec::new();
    let mut cursor = MessageCursor::<MontageEvent>::default();
    let mut commands = MessageCursor::<MontageCommandRecorded>::default();
    let mut wire: Vec<Vec<u8>> = Vec::new();
    for frame in 0..FRAMES {
        if let Some(mut p) = a.world_mut().get_mut::<MontagePlayer>(rig_a) {
            script(frame, &mut p);
        }
        a.update();
        drain_events(&a, &mut cursor, rig_a, frame, &mut events_a);
        statuses_a.push(a.world().get::<MontagePlayer>(rig_a).expect("player").instances());
        for recorded in commands.read(a.world().resource::<Messages<MontageCommandRecorded>>()) {
            assert_eq!(recorded.entity, rig_a);
            wire.push(postcard::to_allocvec(&recorded.command).expect("a command encodes"));
        }
    }
    // Every scripted call is one command (the set_next_section, set_rate and jump are on live
    // instances, so they all apply).
    assert_eq!(wire.len(), 9, "one command per scripted call");
    assert!(
        events_a.iter().any(|e| e.3 == "notify hit"),
        "the script plays through the notify: {events_a:?}"
    );

    // The replaying app: no script, the stream arrives as bytes before the first update.
    let mut b = app();
    let rig_b = spawn_rig(&mut b, player());
    for bytes in &wire {
        let command: TimedMontageCommand = postcard::from_bytes(bytes).expect("a command decodes");
        b.world_mut().write_message(ApplyMontageCommand {
            entity: rig_b,
            command,
        });
    }
    let mut events_b = Vec::new();
    let mut cursor = MessageCursor::<MontageEvent>::default();
    for frame in 0..FRAMES {
        b.update();
        drain_events(&b, &mut cursor, rig_b, frame, &mut events_b);
        let statuses = b.world().get::<MontagePlayer>(rig_b).expect("player").instances();
        assert_eq!(
            statuses, statuses_a[frame as usize],
            "frame {frame}: instances differ"
        );
    }
    assert_eq!(events_b, events_a);
}

/// § 9: the command values themselves: each method's command applied to a fresh player gives
/// the same ids and events as the method call.
#[test]
fn applying_commands_to_a_fresh_player_matches_the_method_calls() {
    let mut recorded = player();
    recorded.set_recording(true);
    let id = recorded.play_named("combo", PlayOptions::default());
    assert!(id.is_some());
    recorded.set_warp_target("enemy", WarpTarget::at(Vec3::X));
    assert!(recorded.set_rate(id, 2.0));
    let commands = recorded.take_commands();
    assert_eq!(commands.len(), 3);
    assert!(matches!(commands[0].command, MontageCommand::Play { .. }));
    let mut fresh = player();
    for command in &commands {
        fresh.apply_command(&command.command).expect("applies");
    }
    assert_eq!(fresh.instances(), recorded.instances());
    assert_eq!(fresh.warp_targets(), recorded.warp_targets());
    assert_eq!(fresh.take_events(), recorded.take_events());
    // A play of a montage the library lacks is refused by name, and starts nothing.
    let refused = fresh.apply_command(&MontageCommand::Play {
        montage: "missing".into(),
        options: PlayOptions::default(),
        queued: false,
    });
    assert!(refused.is_err());
    assert_eq!(fresh.instances().len(), 1);
}

/// M1: a montage naming a slot the player's table lacks warns when the player loads it (the
/// track still plays full body through `play`), and fails the cook check.
#[test]
fn a_montage_with_an_unknown_slot_warns_at_load_and_fails_the_cook_check() {
    let mut p = player();
    assert!(p.load_issues().is_empty());
    let stray = Montage::single("stray", "tail", "strike");
    p.add_montage(stray.clone());
    let issues = p.load_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].code, "montage_unknown_slot");
    assert_eq!(issues[0].track, 0);
    assert!(issues[0].detail.contains("tail"), "{}", issues[0].detail);
    // Replacing it with a fixed montage clears the warning.
    p.add_montage(Montage::single("stray", "upper", "strike"));
    assert!(p.load_issues().is_empty());

    let table = SkeletonSlots::from_ron_str(
        r#"(format_version: 1, slots: [(name: "full_body", group: "body", mask: None), (name: "upper", group: "upper", mask: None)])"#,
    )
    .expect("table");
    let errors = cook_slot_errors(&stray, std::slice::from_ref(&table));
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, "montage_unknown_slot");
    assert!(cook_slot_errors(&combo(), std::slice::from_ref(&table)).is_empty());
    // A project with no slot table defines its slots in code: nothing to check at cook.
    assert!(cook_slot_errors(&stray, &[]).is_empty());
}

/// M4: with `rescale: false` a shorter replacement clip shortens the timeline; sections,
/// notifies and warp windows that now lie past the end are reported by name (a section that
/// starts exactly at the end is empty, not past it).
#[test]
fn a_variant_without_rescale_reports_items_past_the_new_end() {
    let spans = |clip: &str| match clip {
        "strike" => Some(2.0),
        "short" => Some(0.5),
        _ => None,
    };
    let mut lib = MontageLibrary::new();
    assert!(lib.add_montage(combo(), &spans).is_empty());
    let variant = MontageVariant {
        format_version: 1,
        name: "combo_short".into(),
        parent: "combo".into(),
        overrides: vec![SegmentOverride {
            track: 0,
            segment: 0,
            clip: "short".into(),
            rescale: false,
        }],
    };
    let resolved = lib.add_variant(variant, &spans).expect("resolves");
    assert_eq!(resolved.timeline_length(&spans), Some(0.5));
    let past = lib.past_end("combo_short");
    let names: Vec<(&str, &str)> = past.iter().map(|p| (p.kind, p.name.as_str())).collect();
    assert_eq!(
        names,
        vec![("section", "finish"), ("notify", "hit"), ("warp", "enemy")],
        "{past:?}"
    );
    assert!(past.iter().all(|p| p.code == "montage_past_end"));
    // The parent and a rescaled variant keep every item inside the timeline.
    assert!(lib.past_end("combo").is_empty());
    let rescaled = MontageVariant {
        format_version: 1,
        name: "combo_rescaled".into(),
        parent: "combo".into(),
        overrides: vec![SegmentOverride {
            track: 0,
            segment: 0,
            clip: "short".into(),
            rescale: true,
        }],
    };
    lib.add_variant(rescaled, &spans).expect("resolves");
    assert!(lib.past_end("combo_rescaled").is_empty());
}

/// M5: despawning the rig that owns a player cuts its montages: the playing one gets
/// `BlendingOut { interrupted: true }` then `Ended { interrupted: true }`, the queued one a single
/// `Ended { interrupted: true }` and no `Started`.
#[test]
fn despawning_the_owner_cuts_its_montages() {
    let mut app = app();
    let mut p = player();
    let playing = p.play_named("combo", PlayOptions::default()).expect("combo");
    let combo = p.montage("combo").expect("combo").clone();
    let queued = p.queue(combo, PlayOptions::default());
    let rig = spawn_rig(&mut app, p);
    let mut cursor = MessageCursor::<MontageEvent>::default();
    let mut events = Vec::new();
    for frame in 0..5 {
        app.update();
        drain_events(&app, &mut cursor, rig, frame, &mut events);
    }
    events.clear();
    app.world_mut().despawn(rig);
    drain_events(&app, &mut cursor, rig, 5, &mut events);
    let of = |id: u64| -> Vec<String> { events.iter().filter(|e| e.1 == id).map(|e| e.3.clone()).collect() };
    assert_eq!(of(playing.0), vec!["blend_out true", "ended true"], "{events:?}");
    assert_eq!(of(queued.0), vec!["ended true"], "{events:?}");
    // Nothing more arrives for the gone rig.
    app.update();
    let mut later = Vec::new();
    drain_events(&app, &mut cursor, rig, 6, &mut later);
    assert!(later.is_empty(), "{later:?}");
}
