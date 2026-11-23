// SPDX-License-Identifier: MPL-2.0
//! Montage commands over the network (spec 34 § 9, ADR-7216 (1), ADR-9370): a server app and a
//! client app, each with `NetPlugin`, the `AnimationPlugin` and `MontageNetPlugin`, datagrams moved
//! by hand. A gameplay script plays, links, jumps, retimes and stops montages on the server's rig
//! in `FixedUpdate`; the client's copy of the rig plays the same montages on the same server ticks
//! with the same sections and positions, and the same montage events on the same ticks. The
//! server then resends an old command (a duplicate) and an older one (late); the client drops both
//! by their stamps and nothing changes. Time is a step counter; nothing sleeps.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia::montage_net::{MontageCommandMsg, MontageNetPlugin};
// `#[derive(Replicate)]` names `regia_net`; a game that only depends on `regia` aliases it.
use regia::net as regia_net;
use regia::net::model::{LocalPeer, NetMode, PeerId};
use regia::net::replication::register_replicated;
use regia::net::runtime::{NetEntities, NetNode, server_add_peer};
use regia::net::settings::NetSettings;
use regia::net::{
    NetId, NetMessages, NetPlugin, NetSender, NetSet, Replicate, ReplicateOptions, Replicated,
    TransformReplication,
};
use regia_animation::clip::{AnimationClip, JointTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::montage::{MontageSection, SectionNext};
use regia_animation::pose::translated;
use regia_animation::{
    AnimationPlugin, Animator, ClipLibrary, Montage, MontageEvent, MontageEventKind, MontageId,
    MontagePlayer, MontageStatus, PlayOptions, Skeleton, SlotDef, WarpTarget,
};
use regia_app::enable_fixed_step;
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_scene::Transform;

const HZ: f64 = 30.0;
/// Ticks the script and the checks run for.
const TICKS: u32 = 110;
/// The server tick the duplicate goes out on.
const DUPLICATE_TICK: u32 = 50;
/// The server tick the late (older) command goes out on.
const LATE_TICK: u32 = 60;

/// Marks the replicated rig.
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Rig;

fn skeleton() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["root".into(), "arm".into()],
        vec![None, Some(0)],
        vec![translated(Vec3::ZERO), translated(Vec3::Y)],
    )
    .expect("valid skeleton")
}

/// A clip of `duration` seconds: the arm rises.
fn strike(name: &str, duration: f32) -> AnimationClip {
    let mut clip = AnimationClip::empty(name, duration);
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
    clip
}

/// Sections `windup` 0–0.5, `loop` 0.5–1.0 (loops itself), `finish` 1.0–2.0 (stop).
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
    m
}

fn player(recording: bool) -> MontagePlayer {
    let mut clips = ClipLibrary::new();
    clips.insert(strike("strike", 2.0));
    clips.insert(strike("short", 0.5));
    let mut player = MontagePlayer::new(
        vec![
            SlotDef::full_body("full_body", "body"),
            SlotDef::masked("upper", "upper", &["arm"]),
        ],
        clips,
    );
    player.add_montage(combo());
    let mut wave = Montage::single("wave", "upper", "short");
    wave.blend_in.time = 0.05;
    player.add_montage(wave);
    player.set_recording(recording);
    player
}

/// Gives `entity` a skeleton, an animator and a montage player.
fn attach_rig(world: &mut World, entity: Entity, recording: bool) {
    let binding = spawn_skeleton(world, Arc::new(skeleton()));
    let idle = AnimationClip::empty("idle", 1.0);
    world.entity_mut(entity).insert((
        Transform::default(),
        binding,
        Animator::single(idle, true),
        player(recording),
    ));
}

/// The server's gameplay: calls on the rig's player in `FixedUpdate`, by server tick.
fn script(node: Res<NetNode>, mut players: Query<&mut MontagePlayer, With<Rig>>) {
    let tick = node.tick.0;
    for mut player in &mut players {
        match tick {
            10 => {
                player.set_warp_target("enemy", WarpTarget::at(Vec3::new(1.0, 0.0, -2.0)));
                let _ = player.play_named("combo", PlayOptions::default());
            }
            20 => {
                let _ = player.play_named("wave", PlayOptions::default());
            }
            28 => {
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
            80 => {
                let _ = player.stop(None, Some(0.1));
            }
            _ => {}
        }
    }
}

/// Every command the server sent, as its own broadcast copy came back (the server is a receiver
/// of its broadcasts, which the plugin does not apply there).
#[derive(Resource, Default)]
struct SentLog(Vec<MontageCommandMsg>);

fn log_sent(messages: Res<NetMessages<MontageCommandMsg>>, mut log: ResMut<SentLog>) {
    log.0.extend(messages.iter().map(|r| r.msg.clone()));
}

/// Whether the test resends old commands (the duplicate and the late one).
#[derive(Resource)]
struct Resend(bool);

/// Resends the last command sent so far (the jump of tick 45, a duplicate) on
/// [`DUPLICATE_TICK`] and an older one (the wave of tick 20, late) on [`LATE_TICK`].
fn resend(resend: Res<Resend>, log: Res<SentLog>, mut sender: NetSender, rigs: Query<Entity, With<Rig>>) {
    if !resend.0 {
        return;
    }
    // `NetSender` holds the node mutably; its tick is the one being simulated.
    let index = match sender.tick().0 {
        DUPLICATE_TICK => 5,
        LATE_TICK => 2,
        _ => return,
    };
    let Some(msg) = log.0.get(index).cloned() else {
        return;
    };
    for rig in &rigs {
        sender.broadcast(rig, msg.clone()).expect("the rig is replicated");
    }
}

fn setup(world: &mut World) {
    register_replicated::<Rig>(world, ReplicateOptions::default());
}

fn peer_app(mode: NetMode, peer: PeerId) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        NetPlugin { mode },
        AnimationPlugin,
        MontageNetPlugin,
    ));
    enable_fixed_step(&mut app, HZ);
    app.insert_resource(LocalPeer(Some(peer)));
    // A frame every tick, so each client frame applies exactly the server's tick of that frame.
    app.world_mut()
        .resource_mut::<NetSettings>()
        .replication
        .send_every_ticks = 1;
    setup(app.world_mut());
    app
}

fn server_app(resend_old: bool) -> (App, Entity) {
    let mut app = peer_app(NetMode::ListenServer, PeerId::SERVER);
    app.init_resource::<SentLog>()
        .insert_resource(Resend(resend_old))
        .add_systems(FixedUpdate, (script, resend).chain().in_set(NetSet::Gameplay))
        .add_systems(
            FixedUpdate,
            log_sent.after(NetSet::Apply).before(NetSet::Gameplay),
        );
    let rig = app
        .world_mut()
        .spawn((
            Rig,
            Replicated {
                transform: TransformReplication::None,
                ..Replicated::default()
            },
        ))
        .id();
    attach_rig(app.world_mut(), rig, true);
    (app, rig)
}

/// One frame on both apps; datagrams cross at once.
fn pump(server: &mut App, client: &mut App) {
    server.update();
    let down = std::mem::take(&mut server.world_mut().resource_mut::<NetNode>().outbox);
    for mut d in down {
        if d.peer == PeerId(1) {
            d.peer = PeerId::SERVER;
            client.world_mut().resource_mut::<NetNode>().inbox.push(d);
        }
    }
    client.update();
    let up = std::mem::take(&mut client.world_mut().resource_mut::<NetNode>().outbox);
    let mut node = server.world_mut().resource_mut::<NetNode>();
    node.inbox.extend(up.into_iter().map(|mut d| {
        d.peer = PeerId(1);
        d
    }));
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

/// What one peer showed on one tick: the instances after evaluation and that frame's events.
type Frame = (Vec<MontageStatus>, Vec<(u64, String, String)>);

#[derive(Default)]
struct Run {
    /// By server tick.
    server: BTreeMap<u32, Frame>,
    /// By the server tick the client applied in the frame.
    client: BTreeMap<u32, Frame>,
    server_rig: Option<Entity>,
    client_rig: Option<Entity>,
}

fn events(app: &App, rig: Entity, cursor: &mut MessageCursor<MontageEvent>) -> Vec<(u64, String, String)> {
    let messages = app.world().resource::<Messages<MontageEvent>>();
    cursor
        .read(messages)
        .filter(|e| e.entity == rig)
        .map(|e| (e.id.0, e.montage.clone(), kind(&e.kind)))
        .collect()
}

fn run(resend_old: bool) -> (Run, App, App) {
    let (mut server, server_rig) = server_app(resend_old);
    let mut client = peer_app(NetMode::Client, PeerId(1));
    server_add_peer(server.world_mut(), PeerId(1));
    let mut out = Run {
        server_rig: Some(server_rig),
        ..Run::default()
    };
    let mut server_events = Default::default();
    let mut client_events = Default::default();
    for _ in 0..TICKS + 10 {
        pump(&mut server, &mut client);
        // The server's node is on the next tick after its frame; the frame simulated the one
        // before.
        let server_tick = server.world().resource::<NetNode>().tick.0 - 1;
        let statuses = server
            .world()
            .get::<MontagePlayer>(server_rig)
            .expect("server player")
            .instances();
        let ev = events(&server, server_rig, &mut server_events);
        out.server.insert(server_tick, (statuses, ev));

        // The client's copy of the rig gets its player as soon as it is replicated.
        if out.client_rig.is_none() {
            let id = *server.world().get::<NetId>(server_rig).expect("assigned");
            if let Some(entity) = client.world().resource::<NetEntities>().entity(id) {
                attach_rig(client.world_mut(), entity, false);
                out.client_rig = Some(entity);
            }
            continue;
        }
        let rig = out.client_rig.expect("attached");
        let client_tick = client.world().resource::<NetNode>().tick.0;
        let statuses = client
            .world()
            .get::<MontagePlayer>(rig)
            .expect("client player")
            .instances();
        let ev = events(&client, rig, &mut client_events);
        out.client.insert(client_tick, (statuses, ev));
    }
    (out, server, client)
}

/// The client plays every montage on the server's tick with the same sections, positions and
/// events.
#[test]
fn a_montage_played_on_the_server_plays_on_the_client_at_the_same_tick() {
    let (run, server, client) = run(false);
    let client_rig = run.client_rig.expect("the rig replicated");
    let first = *run.client.keys().next().expect("client frames");
    assert!(
        first < 10,
        "the client has its rig before the first play: tick {first}"
    );
    let mut playing = 0;
    for tick in first..TICKS {
        let server_frame = run
            .server
            .get(&tick)
            .unwrap_or_else(|| panic!("server tick {tick}"));
        let client_frame = run
            .client
            .get(&tick)
            .unwrap_or_else(|| panic!("client tick {tick}"));
        assert_eq!(
            client_frame, server_frame,
            "tick {tick}: the client differs from the server"
        );
        if !server_frame.0.is_empty() {
            playing += 1;
        }
    }
    assert!(playing > 60, "montages play for most of the run: {playing} ticks");
    let started = run.server.get(&10).expect("tick 10");
    assert_eq!(started.0[0].montage, "combo");
    assert!(
        run.server
            .values()
            .any(|f| f.1.iter().any(|e| e.2 == "section windup->loop")),
        "the script passes a section link"
    );
    let sent = *server
        .world()
        .get::<MontagePlayer>(run.server_rig.expect("server rig"))
        .expect("player")
        .net_state();
    let received = *client
        .world()
        .get::<MontagePlayer>(client_rig)
        .expect("player")
        .net_state();
    assert_eq!(sent.sent, 8, "one message per scripted call: {sent:?}");
    assert_eq!(received.received, 8, "{received:?}");
    assert_eq!(received.dropped, 0, "{received:?}");
}

/// A duplicated message and a late (older) one change nothing on the client: both are dropped
/// by their stamps, and the client still matches the server tick for tick.
#[test]
fn a_duplicated_or_late_message_changes_nothing() {
    let (plain, _, _) = run(false);
    let (resent, server, client) = run(true);
    assert_eq!(
        server.world().resource::<SentLog>().0.len(),
        10,
        "8 commands, then 2 resends"
    );
    for tick in DUPLICATE_TICK - 1..TICKS {
        assert_eq!(
            resent.client.get(&tick),
            resent.server.get(&tick),
            "tick {tick}: the client differs from the server"
        );
        assert_eq!(
            resent.client.get(&tick),
            plain.client.get(&tick),
            "tick {tick}: the resends changed the client"
        );
    }
    let state = *client
        .world()
        .get::<MontagePlayer>(resent.client_rig.expect("client rig"))
        .expect("player")
        .net_state();
    assert_eq!(state.received, 8, "{state:?}");
    assert_eq!(state.dropped, 2, "the duplicate and the late command: {state:?}");
}
