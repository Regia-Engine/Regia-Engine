// SPDX-License-Identifier: MPL-2.0
//! The replay player (spec 103 § 12): a loopback session recorded on the server and played back in a world with no
//! server must show, tick by tick, what a live non-owner client showed. No wall clock; everything runs in fixed steps.

use std::collections::BTreeMap;
use std::path::PathBuf;

use regia_app::prelude::*;
use regia_ecs::ecs::system::RunSystemOnce;
use regia_framework::net_replay::{
    NetReplayPlayer, NetReplayPlugin, ReplayCamera, ReplayOptions, close_replay, open_replay, replay_state,
};
use regia_framework::spectator::{SpectatorDrive, SpectatorPlugin};
use regia_net::interp::{InterpolationMode, NetInterpolation};
use regia_net::message::{NetMessages, NetSender, register_net_message};
use regia_net::model::{LocalPeer, NetId, NetMode, NetOwnedLocally, NetOwner, NetTick, PeerId};
use regia_net::plugin::NetPlugin;
use regia_net::recorder::{
    NetDemoError, RECORDER_PEER, RecorderConfig, RecorderView, RingConfig, export_ring, start_recording,
    start_ring, stop_recording,
};
use regia_net::relevance::{NetVisibility, install_net_interest};
use regia_net::replication::{
    Audience, NetPrefabSpawner, NetRelevance, ReplicateOptions, Replicated, SpawnKind, TransformReplication,
    register_replicated,
};
use regia_net::runtime::{NetEntities, NetNode, ensure_resources, net_apply, net_receive, net_send};
use regia_net::settings::{NetSettings, StateChannel};
use regia_net::testing::LoopbackHub;
use regia_net::*;
use regia_scene::components::EntityGuid;
use regia_scene::level::LevelMember;
use regia_scene::prefab::PrefabInstance;
use regia_scene::transform::Transform;

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Health {
    hp: i32,
    #[replicate(max_len = 64)]
    label: String,
}

/// Only its owner may see it.
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Secret {
    code: u32,
}

/// Fixed at spawn for everyone.
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Birth {
    day: u32,
}

/// An entity reference.
#[derive(Component, Clone, Debug, PartialEq, Replicate)]
struct Aim {
    target: Entity,
}

/// For everyone but the owner.
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Seen {
    n: u32,
}

#[derive(Clone, Debug, PartialEq, NetMessage)]
#[net(direction = "broadcast", id = "test.boom")]
struct Boom {
    power: u32,
}

#[derive(Clone, Debug, PartialEq, NetMessage)]
#[net(direction = "broadcast", channel = "unreliable", id = "test.spark")]
struct Spark {
    n: u32,
}

#[derive(Clone, Debug, PartialEq, NetMessage)]
#[net(direction = "to_owner", id = "test.whisper")]
struct Whisper {
    n: u32,
}

fn opts(audience: Audience) -> ReplicateOptions {
    ReplicateOptions {
        audience,
        ..ReplicateOptions::default()
    }
}

fn setup(world: &mut World) {
    register_replicated::<Health>(world, ReplicateOptions::default());
    register_replicated::<Secret>(world, opts(Audience::OwnerOnly));
    register_replicated::<Birth>(world, opts(Audience::SpawnOnly));
    register_replicated::<Aim>(world, ReplicateOptions::default());
    register_replicated::<Seen>(world, opts(Audience::Others));
    register_net_message::<Boom>(world);
    register_net_message::<Spark>(world);
    register_net_message::<Whisper>(world);
}

fn setup_other(world: &mut World) {
    register_replicated::<Secret>(world, opts(Audience::OwnerOnly));
    register_replicated::<Health>(world, ReplicateOptions::default());
}

fn temp_file(tag: &str) -> PathBuf {
    regia_test_utils::unique_temp_dir(tag).join("match.netdemo")
}

/// A digest of what a world holds of the components every non-owner sees: one row per copy, in id order.
fn digest(world: &mut World) -> String {
    let mut q = world.query::<(
        &NetId,
        Option<&Health>,
        Option<&Transform>,
        Option<&Seen>,
        Option<&Aim>,
    )>();
    let map = world.resource::<NetEntities>().clone();
    let mut rows = BTreeMap::new();
    for (id, h, t, seen, aim) in q.iter(world) {
        let pos = t.map(|t| t.translation.to_array());
        let aimed = aim.map(|a| map.net_id(a.target).map(|i| i.0));
        rows.insert(id.0, format!("{h:?} {pos:?} {seen:?} {aimed:?}"));
    }
    format!("{rows:?}")
}

/// A profile relevant everywhere, so a player with no viewpoint holds what the visibility lists give it.
fn install_world_profile(world: &mut World) {
    install_net_interest(world);
    world
        .resource_mut::<NetProfiles>()
        .insert(
            "world",
            NetProfile {
                relevance: RelevanceShape::World,
                ..NetProfile::default()
            },
        )
        .expect("a valid profile");
}

fn world_profile() -> NetProfileRef {
    NetProfileRef("world".to_string())
}

fn replay_world(setup: fn(&mut World)) -> World {
    let mut w = World::new();
    w.insert_resource(NetMode::Client);
    w.insert_resource(LocalPeer(None));
    let mut settings = NetSettings::default();
    settings.replication.send_every_ticks = 1;
    settings.replication.state_channel = StateChannel::Reliable;
    w.insert_resource(settings);
    setup(&mut w);
    ensure_resources(&mut w);
    w
}

/// One fixed tick of a playback world, in the plugin's order.
fn replay_tick(world: &mut World, player: &mut NetReplayPlayer, ticks: f64) {
    player.advance_ticks(world, ticks);
    net_receive(world);
    net_apply(world);
    player.after_apply(world);
    regia_net::predict::net_predict(world);
    net_send(world);
    player.after_send(world);
}

fn applied(world: &World) -> NetTick {
    world.resource::<NetNode>().tick
}

/// What a client delivered in one tick.
fn messages_of(world: &World) -> Vec<String> {
    let map = world.resource::<NetEntities>().clone();
    let mut out = Vec::new();
    if let Some(m) = world.get_resource::<NetMessages<Boom>>() {
        for r in m.iter() {
            let target = r.target.and_then(|e| map.net_id(e)).map(|i| i.0);
            out.push(format!(
                "boom {} on {target:?} from tick {}",
                r.msg.power, r.sent_tick.0
            ));
        }
    }
    if let Some(m) = world.get_resource::<NetMessages<Spark>>() {
        for r in m.iter() {
            out.push(format!("spark {}", r.msg.n));
        }
    }
    if let Some(m) = world.get_resource::<NetMessages<Whisper>>() {
        for r in m.iter() {
            out.push(format!("whisper {}", r.msg.n));
        }
    }
    out.sort();
    out
}

/// Runs a scripted server tick: spawns, despawns, value changes, ownership changes and messages.
struct Script {
    alive: Vec<Entity>,
    owner: PeerId,
    tick: u32,
}

impl Script {
    fn new(owner: PeerId) -> Self {
        Self {
            alive: Vec::new(),
            owner,
            tick: 0,
        }
    }

    fn step(&mut self, hub: &mut LoopbackHub) {
        let t = self.tick;
        self.tick += 1;
        let w = &mut hub.server.world;
        if t.is_multiple_of(7) {
            let e = w
                .spawn((
                    Replicated::default(),
                    Transform::from_translation(regia_core::math::Vec3::new(t as f32, 0.0, 1.0)),
                    Health {
                        hp: t as i32,
                        label: format!("unit {t}"),
                    },
                    Secret { code: t * 3 },
                    Seen { n: t },
                ))
                .id();
            self.alive.push(e);
        }
        if t % 11 == 5 && self.alive.len() > 2 {
            let e = self.alive.remove(1);
            let _ = w.despawn(e);
        }
        if t % 13 == 3
            && let Some(e) = self.alive.last().copied()
        {
            w.entity_mut(e).insert(NetOwner(self.owner));
        }
        if t % 17 == 4
            && let Some(e) = self.alive.last().copied()
        {
            w.entity_mut(e).insert(NetOwner(PeerId::SERVER));
        }
        for (i, e) in self.alive.iter().enumerate() {
            if (t as usize + i).is_multiple_of(3) {
                if let Some(mut h) = w.get_mut::<Health>(*e) {
                    h.hp += 1;
                }
                if let Some(mut tr) = w.get_mut::<Transform>(*e) {
                    tr.translation.y += 0.5;
                }
                if let Some(mut s) = w.get_mut::<Secret>(*e) {
                    s.code += 1;
                }
            }
        }
        if t > 3 && t.is_multiple_of(5) {
            let target = self
                .alive
                .last()
                .copied()
                .filter(|e| w.get::<NetId>(*e).is_some());
            let owner = self.owner;
            w.run_system_once(move |mut net: NetSender| {
                if let Some(e) = target {
                    let _ = net.broadcast(e, Boom { power: t });
                    let _ = net.broadcast(e, Spark { n: t });
                }
                let _ = net.broadcast_session(Boom { power: t + 1000 });
                let _ = net.send_session_to_peer(owner, Whisper { n: t });
            })
            .expect("system runs");
        }
        hub.step();
    }
}

struct Recorded {
    path: PathBuf,
    /// The live observer's digest at each tick.
    live: BTreeMap<u32, String>,
    /// What the live observer delivered, by the tick it was applied at.
    live_messages: BTreeMap<u32, Vec<String>>,
    first: u32,
    last: u32,
}

/// Records `ticks` of the script. Client 0 gets ownership changes; client 1 only ever watches.
fn record_session(tag: &str, ticks: usize, config: RecorderConfig) -> Recorded {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let owner = hub.add_client();
    let observer = hub.add_client();
    let path = temp_file(tag);
    start_recording(&mut hub.server.world, &path, config).expect("starts");
    let mut script = Script::new(hub.clients[owner].peer);
    let mut live = BTreeMap::new();
    let mut live_messages: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for _ in 0..ticks {
        script.step(&mut hub);
        let w = &mut hub.clients[observer].world;
        let tick = applied(w).0;
        live.insert(tick, digest(w));
        let m = messages_of(w);
        if !m.is_empty() {
            live_messages.entry(tick).or_default().extend(m);
        }
    }
    // Two more steps: the observer receives what the server sent last.
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    for _ in 0..2 {
        hub.step();
        let w = &mut hub.clients[observer].world;
        let tick = applied(w).0;
        live.entry(tick).or_insert_with(|| digest(w));
        let m = messages_of(w);
        if !m.is_empty() {
            live_messages.entry(tick).or_default().extend(m);
        }
    }
    Recorded {
        path,
        live,
        live_messages,
        first: summary.first_tick.0,
        last: summary.last_tick.0,
    }
}

fn plain_options() -> ReplayOptions {
    ReplayOptions {
        interpolate: false,
        ..ReplayOptions::default()
    }
}

/// Spec 103 § 12.1 and § 12.3: a 300-tick session with spawns, despawns, ownership changes and messages. Playback's
/// per-tick digest equals the live non-owner client's at every tick, it delivers the same messages at the same ticks, and
/// what only an owner receives is not in the recording.
#[test]
fn playback_shows_what_a_live_client_showed_tick_by_tick() {
    let rec = record_session("net_replay_digest", 300, RecorderConfig::default());
    assert_eq!(rec.last - rec.first, 299);
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &rec.path, plain_options()).expect("opens");
    assert!(
        player
            .header()
            .components
            .iter()
            .any(|c| c.name.contains("Health"))
    );
    assert_eq!(player.header().view, RecorderView::Everything);
    let mut compared = 0;
    let mut replay_messages: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for t in rec.first..=rec.last {
        replay_tick(&mut world, &mut player, 1.0);
        assert_eq!(applied(&world).0, t, "the playback applied tick {t}");
        let Some(want) = rec.live.get(&t) else {
            continue;
        };
        assert_eq!(&digest(&mut world), want, "tick {t}");
        compared += 1;
        let m = messages_of(&world);
        if !m.is_empty() {
            replay_messages.entry(t).or_default().extend(m);
        }
        let mut owned = world.query_filtered::<(), With<NetOwnedLocally>>();
        assert_eq!(owned.iter(&world).count(), 0, "a recorder owns nothing");
        let mut secrets = world.query::<&Secret>();
        assert_eq!(
            secrets.iter(&world).count(),
            0,
            "owner-only values are not recorded"
        );
    }
    assert!(compared >= 295, "compared {compared} ticks");
    let live: BTreeMap<u32, Vec<String>> = rec
        .live_messages
        .iter()
        .filter(|(t, _)| (rec.first..=rec.last).contains(*t))
        .map(|(t, m)| (*t, m.clone()))
        .collect();
    assert!(!live.is_empty());
    assert_eq!(replay_messages, live, "the same messages at the same ticks");
    let whispers = replay_messages
        .values()
        .flatten()
        .filter(|m| m.starts_with("whisper"))
        .count();
    assert_eq!(whispers, 0, "a message to one peer is not recorded");
    let state = player.state(&world);
    assert!(state.ended);
    assert!(!state.playing);
    assert_eq!(state.resync_unanswered, 0);
    assert_eq!(player.error(), None);
}

/// Spec 103 § 12.5: seeks in any direction land on linear play's digest.
#[test]
fn seeking_backward_and_forward_gives_the_digest_of_linear_play() {
    let rec = record_session(
        "net_replay_seek",
        300,
        RecorderConfig {
            keyframe_every_s: 0.5,
            ..RecorderConfig::default()
        },
    );
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &rec.path, plain_options()).expect("opens");
    let mut linear = BTreeMap::new();
    for t in rec.first..=rec.last {
        replay_tick(&mut world, &mut player, 1.0);
        linear.insert(t, digest(&mut world));
    }
    let f = rec.first;
    // Backward, forward in a segment, forward over segments, onto keyframes, the ends, the same tick twice.
    let probes = [
        f + 250,
        f,
        f + 299,
        f + 31,
        f + 32,
        f + 33,
        f + 58,
        f + 59,
        f + 60,
        f + 61,
        f + 5,
        f + 200,
        f + 120,
        f + 90,
        f + 90,
        f + 150,
        f + 1,
        f + 299,
    ];
    for (i, t) in probes.into_iter().enumerate() {
        player.seek(t).expect("in range");
        replay_tick(&mut world, &mut player, 0.0);
        let state = player.state(&world);
        assert!(!state.seeking, "a short segment lands in one tick");
        assert_eq!(state.tick, t);
        assert_eq!(state.generation, i as u64 + 1);
        assert_eq!(
            digest(&mut world),
            linear[&t],
            "after a seek to tick {t} (seek {i})"
        );
        assert_eq!(messages_of(&world), Vec::<String>::new(), "a seek is silent");
    }
    // Playing on from a seek continues exactly as linear play.
    player.seek(f + 100).expect("in range");
    replay_tick(&mut world, &mut player, 0.0);
    player.play();
    for t in f + 101..=f + 140 {
        replay_tick(&mut world, &mut player, 1.0);
        assert_eq!(digest(&mut world), linear[&t], "playing on at tick {t}");
    }
    assert_eq!(
        player.seek(rec.last + 1).expect_err("past the end").code(),
        "replay_seek_range"
    );
    assert_eq!(
        player.seek_seconds(-1.0).expect_err("before the start").code(),
        "replay_seek_range"
    );
}

/// Spec 103 § 3.5: a keyframe is a join. A `SpawnOnly` value that changed after its spawn stays old in linear play and
/// shows the new one after a seek, as a client joining at that tick sees.
#[test]
fn a_keyframe_shows_a_spawn_only_value_as_a_join_does() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let path = temp_file("net_replay_spawn_only");
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            keyframe_every_s: 0.1,
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    let e = hub
        .server
        .world
        .spawn((Replicated::default(), Birth { day: 1 }))
        .id();
    hub.run(10);
    hub.server.world.get_mut::<Birth>(e).expect("birth").day = 2;
    hub.run(20);
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &path, plain_options()).expect("opens");
    let day = |world: &mut World| {
        let mut q = world.query::<&Birth>();
        q.iter(world).next().map(|b| b.day)
    };
    for _ in summary.first_tick.0..=summary.last_tick.0 {
        replay_tick(&mut world, &mut player, 1.0);
    }
    assert_eq!(
        day(&mut world),
        Some(1),
        "linear play keeps the value of the spawn"
    );
    // Backward, so it goes through a keyframe taken after the change (a seek to where it stands moves nothing).
    player.seek(summary.last_tick.0 - 1).expect("in range");
    replay_tick(&mut world, &mut player, 0.0);
    assert_eq!(day(&mut world), Some(2), "a seek shows what a joiner would get");
}

/// Spec 103 § 12.7.
#[test]
fn another_protocol_is_refused_and_names_the_row() {
    let rec = record_session("net_replay_protocol", 10, RecorderConfig::default());
    let mut world = replay_world(setup_other);
    let err = NetReplayPlayer::open(&mut world, &rec.path, plain_options()).expect_err("refused");
    assert_eq!(err.code(), "netdemo_protocol_mismatch");
    let text = err.to_string();
    assert!(
        text.contains("component 0") && text.contains("Health") && text.contains("Secret"),
        "{text}"
    );
    assert_eq!(
        *world.resource::<NetMode>(),
        NetMode::Client,
        "the world is as it was"
    );
}

#[test]
fn a_world_with_a_session_is_refused() {
    let rec = record_session("net_replay_session", 10, RecorderConfig::default());
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    hub.add_client();
    let err = NetReplayPlayer::open(&mut hub.server.world, &rec.path, plain_options()).expect_err("refused");
    assert_eq!(err.code(), "netdemo_session_active");
}

/// A recording that started mid-session starts from a keyframe, and reads the same as the live client from there.
#[test]
fn a_recording_started_mid_session_starts_from_a_keyframe() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let owner = hub.add_client();
    let observer = hub.add_client();
    let mut script = Script::new(hub.clients[owner].peer);
    for _ in 0..50 {
        script.step(&mut hub);
    }
    let path = temp_file("net_replay_late");
    start_recording(&mut hub.server.world, &path, RecorderConfig::default()).expect("starts");
    let mut live = BTreeMap::new();
    for _ in 0..50 {
        script.step(&mut hub);
        let w = &mut hub.clients[observer].world;
        live.insert(applied(w).0, digest(w));
    }
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &path, plain_options()).expect("opens");
    let mut compared = 0;
    for t in summary.first_tick.0..=summary.last_tick.0 {
        replay_tick(&mut world, &mut player, 1.0);
        if let Some(want) = live.get(&t) {
            // Ids a live client got in the first 50 ticks are in the keyframe: the rows match.
            assert_eq!(&digest(&mut world), want, "tick {t}");
            compared += 1;
        }
    }
    assert!(compared > 40);
}

/// Spec 103 § 12.8: a ring export plays and matches the live client from its first keyframe on.
#[test]
fn a_ring_export_plays_like_the_live_stream() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let owner = hub.add_client();
    let observer = hub.add_client();
    start_ring(
        &mut hub.server.world,
        RingConfig {
            seconds: 1.0,
            keyframe_every_s: 0.25,
            max_bytes: 1 << 20,
            ..RingConfig::default()
        },
    )
    .expect("starts");
    let mut script = Script::new(hub.clients[owner].peer);
    let mut live = BTreeMap::new();
    for _ in 0..400 {
        script.step(&mut hub);
        let w = &mut hub.clients[observer].world;
        live.insert(applied(w).0, digest(w));
    }
    let path = temp_file("net_replay_ring");
    let summary = export_ring(&mut hub.server.world, &path, Some(0.5), false).expect("exports");
    assert!(summary.last_tick.0 - summary.first_tick.0 >= 30);
    assert!(summary.last_tick.0 - summary.first_tick.0 < 60 + 16);
    // Two more steps so the observer has the last ticks the ring holds.
    for _ in 0..2 {
        script.step(&mut hub);
        let w = &mut hub.clients[observer].world;
        live.insert(applied(w).0, digest(w));
    }
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &path, plain_options()).expect("opens");
    assert_eq!(player.header().first_tick, summary.first_tick);
    for t in summary.first_tick.0..=summary.last_tick.0 {
        replay_tick(&mut world, &mut player, 1.0);
        assert_eq!(digest(&mut world), live[&t], "tick {t}");
    }
    assert!(player.state(&world).ended);
    // The ring keeps running.
    assert!(export_ring(&mut hub.server.world, &temp_file("net_replay_ring2"), None, false).is_ok());
}

/// Spec 103 § 3.3 and § 12.4: a `Player` view holds that peer's set at each tick, and `Everything` holds all of it.
#[test]
fn a_player_view_holds_what_that_player_holds() {
    /// Each live player's held ids by tick, the file, and its first and last ticks.
    type ViewRun = (
        BTreeMap<u32, Vec<u32>>,
        BTreeMap<u32, Vec<u32>>,
        PathBuf,
        u32,
        u32,
    );

    fn run(view: RecorderView) -> ViewRun {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
        let a = hub.add_client();
        let b = hub.add_client();
        let pa = hub.clients[a].peer;
        let pb = hub.clients[b].peer;
        install_world_profile(&mut hub.server.world);
        hub.server
            .world
            .resource_mut::<NetSettings>()
            .replication
            .client_rate_bytes = 10 << 20;
        let path = temp_file("net_replay_view");
        let view = match view {
            RecorderView::Player(_) => RecorderView::Player(pa),
            v => v,
        };
        start_recording(
            &mut hub.server.world,
            &path,
            RecorderConfig {
                view,
                keyframe_every_s: 0.2,
                ..RecorderConfig::default()
            },
        )
        .expect("starts");
        // Relevant everywhere: the visibility lists decide who holds what.
        let shared = hub
            .server
            .world
            .spawn((Replicated::default(), Health::default(), world_profile()))
            .id();
        let only_a = hub
            .server
            .world
            .spawn((
                Replicated::default(),
                Health::default(),
                world_profile(),
                NetVisibility {
                    allow: vec![pa],
                    deny: Vec::new(),
                },
            ))
            .id();
        let only_b = hub
            .server
            .world
            .spawn((
                Replicated::default(),
                Health::default(),
                world_profile(),
                NetVisibility {
                    allow: vec![pb],
                    deny: Vec::new(),
                },
            ))
            .id();
        let _ = (shared, only_a, only_b);
        let held = |w: &mut World| {
            let mut q = w.query::<&NetId>();
            let mut v: Vec<u32> = q.iter(w).map(|i| i.0).collect();
            v.sort_unstable();
            v
        };
        let mut live_a = BTreeMap::new();
        let mut live_b = BTreeMap::new();
        for t in 0..90 {
            hub.step();
            if t == 40 {
                // Leaves a's set: visible to b now.
                hub.server.world.entity_mut(only_a).insert(NetVisibility {
                    allow: vec![pb],
                    deny: Vec::new(),
                });
            }
            let w = &mut hub.clients[a].world;
            live_a.insert(applied(w).0, held(w));
            let w = &mut hub.clients[b].world;
            live_b.insert(applied(w).0, held(w));
        }
        let summary = stop_recording(&mut hub.server.world).expect("stops");
        (live_a, live_b, path, summary.first_tick.0, summary.last_tick.0)
    }

    let (live_a, _, path, first, last) = run(RecorderView::Player(PeerId(1)));
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &path, plain_options()).expect("opens");
    assert_eq!(player.header().view, RecorderView::Player(PeerId(1)));
    let mut changes = 0;
    let mut previous: Option<Vec<u32>> = None;
    for t in first..=last {
        replay_tick(&mut world, &mut player, 1.0);
        let Some(want) = live_a.get(&t) else {
            continue;
        };
        let mut q = world.query::<&NetId>();
        let mut got: Vec<u32> = q.iter(&world).map(|i| i.0).collect();
        got.sort_unstable();
        assert_eq!(&got, want, "tick {t}: the same set as player 1");
        if previous.as_ref().is_some_and(|p| *p != got) {
            changes += 1;
        }
        previous = Some(got);
    }
    assert!(changes >= 1, "the set changed during the recording");

    let (_, _, path, first, last) = run(RecorderView::Everything);
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &path, plain_options()).expect("opens");
    for _ in first..=last {
        replay_tick(&mut world, &mut player, 1.0);
    }
    let mut q = world.query::<&NetId>();
    assert_eq!(q.iter(&world).count(), 3, "everything holds all three");
}

/// A departed player's view freezes: what it held keeps updating, nothing new enters.
#[test]
fn a_departed_players_view_freezes() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    install_world_profile(&mut hub.server.world);
    let path = temp_file("net_replay_view_gone");
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            view: RecorderView::Player(pa),
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    let held = hub
        .server
        .world
        .spawn((
            Replicated::default(),
            Health::default(),
            world_profile(),
            NetVisibility {
                allow: vec![pa],
                deny: Vec::new(),
            },
        ))
        .id();
    hub.run(5);
    regia_net::runtime::server_remove_peer(&mut hub.server.world, pa);
    hub.run(2);
    assert!(regia_net::recorder::recorder_status(&hub.server.world).view_peer_gone);
    hub.server.world.get_mut::<Health>(held).expect("health").hp = 42;
    // Relevant to everyone there is; the frozen view still keeps it out.
    hub.server
        .world
        .spawn((Replicated::default(), Health::default(), world_profile()));
    hub.run(5);
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &path, plain_options()).expect("opens");
    for _ in summary.first_tick.0..=summary.last_tick.0 {
        replay_tick(&mut world, &mut player, 1.0);
    }
    let mut q = world.query::<&Health>();
    let values: Vec<i32> = q.iter(&world).map(|h| h.hp).collect();
    assert_eq!(values, vec![42], "what it held updated, the newcomer stayed out");
}

/// Spec 103 § 12.12 and § 3.6: with no server to answer, references still resolve and clear as on a live client.
#[test]
fn references_resolve_and_clear_with_no_server_to_ask() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let observer = hub.add_client();
    let path = temp_file("net_replay_refs");
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            keyframe_every_s: 0.2,
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    // The referrer has the lower id, so its record comes first in the frame that spawns both.
    let referrer = hub
        .server
        .world
        .spawn((
            Replicated::default(),
            Aim {
                target: Entity::PLACEHOLDER,
            },
        ))
        .id();
    hub.server.world.flush();
    let target = hub
        .server
        .world
        .spawn((Replicated::default(), Health::default()))
        .id();
    hub.server.world.get_mut::<Aim>(referrer).expect("aim").target = target;
    let quiet = hub
        .server
        .world
        .spawn((
            Replicated::default(),
            Aim {
                target: Entity::PLACEHOLDER,
            },
        ))
        .id();
    hub.run(30);
    let aim_of = |world: &World, map_id: NetId| -> Option<Option<u32>> {
        let map = world.resource::<NetEntities>();
        let e = map.entity(map_id)?;
        let a = world.get::<Aim>(e)?;
        Some(map.net_id(a.target).map(|i| i.0))
    };
    let referrer_id = *hub.server.world.get::<NetId>(referrer).expect("id");
    let target_id = *hub.server.world.get::<NetId>(target).expect("id");
    assert_eq!(
        aim_of(&hub.clients[observer].world, referrer_id),
        Some(Some(target_id.0)),
        "live client"
    );
    let _ = hub.server.world.despawn(target);
    let despawn_at = hub.server_tick().0;
    hub.run(30);
    assert_eq!(
        aim_of(&hub.clients[observer].world, referrer_id),
        Some(None),
        "live client cleared it"
    );
    let _ = quiet;
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &path, plain_options()).expect("opens");
    let first = summary.first_tick.0;
    for _ in first..despawn_at {
        replay_tick(&mut world, &mut player, 1.0);
    }
    assert_eq!(
        aim_of(&world, referrer_id),
        Some(Some(target_id.0)),
        "resolved in linear play"
    );
    for _ in despawn_at..=summary.last_tick.0 {
        replay_tick(&mut world, &mut player, 1.0);
    }
    assert_eq!(aim_of(&world, referrer_id), Some(None), "cleared in linear play");
    // A seek into the same stretch: the keyframe's two passes resolve it.
    player.seek(despawn_at - 5).expect("in range");
    replay_tick(&mut world, &mut player, 0.0);
    assert_eq!(
        aim_of(&world, referrer_id),
        Some(Some(target_id.0)),
        "resolved after a seek"
    );
    player.seek(summary.last_tick.0).expect("in range");
    replay_tick(&mut world, &mut player, 0.0);
    assert_eq!(aim_of(&world, referrer_id), Some(None), "cleared after a seek");
}

#[derive(Component, Debug)]
struct FromPrefab;

/// Spec 103 § 12.13: copies come from the world's prefab spawner, including after a seek.
#[test]
fn prefab_copies_come_from_the_worlds_spawner() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let path = temp_file("net_replay_prefab");
    start_recording(&mut hub.server.world, &path, RecorderConfig::default()).expect("starts");
    hub.server.world.spawn((
        Replicated::default(),
        Health::default(),
        PrefabInstance {
            prefab: 77,
            ..PrefabInstance::default()
        },
    ));
    hub.run(5);
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    assert!(matches!(
        hub.server
            .world
            .resource::<regia_net::replication::ServerReplication>()
            .entities()
            .next()
            .map(|(_, _, _, k)| k),
        Some(SpawnKind::Prefab(77))
    ));
    let mut world = replay_world(setup);
    world.insert_resource(NetPrefabSpawner(Box::new(|w, id| {
        assert_eq!(id, 77);
        Some(w.spawn(FromPrefab).id())
    })));
    let mut player = NetReplayPlayer::open(&mut world, &path, plain_options()).expect("opens");
    let count = |world: &mut World| {
        let mut q = world.query_filtered::<&Health, With<FromPrefab>>();
        q.iter(world).count()
    };
    for _ in summary.first_tick.0..=summary.last_tick.0 {
        replay_tick(&mut world, &mut player, 1.0);
    }
    assert_eq!(count(&mut world), 1);
    player.seek(summary.first_tick.0).expect("in range");
    replay_tick(&mut world, &mut player, 0.0);
    assert_eq!(count(&mut world), 1, "a seek made it again, by the prefab");
    let mut stale = world.query_filtered::<(), (With<FromPrefab>, Without<NetId>)>();
    assert_eq!(stale.iter(&world).count(), 0, "and the old copy went");
}

fn restore_level(world: &mut World, level: &str) {
    assert_eq!(level, "arena");
    let guid = EntityGuid(regia_core::id::Guid::from_u128(5));
    let mut q = world.query_filtered::<Entity, With<LevelMember>>();
    if q.iter(world).next().is_none() {
        world.spawn((LevelMember(9), guid, Health::default()));
    }
}

/// Spec 103 § 8.1: a level-bound copy binds as live; a backward seek past its removal gets it back through the host's hook.
#[test]
fn a_level_bound_entity_comes_back_through_the_hook() {
    let guid = EntityGuid(regia_core::id::Guid::from_u128(5));
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let path = temp_file("net_replay_level");
    let door = hub
        .server
        .world
        .spawn((
            Replicated::default(),
            LevelMember(9),
            guid,
            Health {
                hp: 7,
                ..Health::default()
            },
        ))
        .id();
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            level: "arena".to_string(),
            keyframe_every_s: 0.1,
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    hub.run(20);
    let _ = hub.server.world.despawn(door);
    hub.run(20);
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    let mut world = replay_world(setup);
    world.spawn((LevelMember(9), guid, Health::default()));
    let mut player = NetReplayPlayer::open(
        &mut world,
        &path,
        ReplayOptions {
            interpolate: false,
            level_restore: Some(restore_level),
            ..ReplayOptions::default()
        },
    )
    .expect("opens");
    assert_eq!(player.header().level, "arena");
    let doors = |world: &mut World| {
        let mut q = world.query_filtered::<&Health, With<LevelMember>>();
        q.iter(world).map(|h| h.hp).collect::<Vec<_>>()
    };
    for _ in 0..5 {
        replay_tick(&mut world, &mut player, 1.0);
    }
    assert_eq!(
        doors(&mut world),
        vec![7],
        "bound by guid, the recorded value on top"
    );
    for _ in summary.first_tick.0 + 5..=summary.last_tick.0 {
        replay_tick(&mut world, &mut player, 1.0);
    }
    assert_eq!(doors(&mut world), Vec::<i32>::new(), "the recording removed it");
    player.seek(summary.first_tick.0 + 5).expect("in range");
    replay_tick(&mut world, &mut player, 0.0);
    assert_eq!(
        doors(&mut world),
        vec![7],
        "the hook restored it and the keyframe bound it"
    );
    // Closed where the recording had removed it: the world gets its level back.
    player.seek(summary.last_tick.0).expect("in range");
    replay_tick(&mut world, &mut player, 0.0);
    assert_eq!(doors(&mut world), Vec::<i32>::new());
    player.close(&mut world);
    assert_eq!(doors(&mut world), vec![0], "the hook ran at the close");
}

/// Spec 103 § 12.6: a file damaged after the open stops the player with a typed error.
#[test]
fn damage_after_the_open_stops_the_player_with_a_typed_error() {
    let rec = record_session("net_replay_damage", 40, RecorderConfig::default());
    let bytes = std::fs::read(&rec.path).expect("read");
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &rec.path, plain_options()).expect("opens");
    for _ in 0..5 {
        replay_tick(&mut world, &mut player, 1.0);
    }
    let mut bad = bytes;
    let n = bad.len();
    bad[n - 100] ^= 0xff;
    std::fs::write(&rec.path, &bad).expect("write");
    for _ in 0..60 {
        replay_tick(&mut world, &mut player, 1.0);
    }
    let err = player.error().expect("stopped");
    assert_eq!(err.code(), "netdemo_corrupt", "{err}");
    assert!(!player.state(&world).playing);
    assert!(player.state(&world).error.is_some());
}

/// A record whose checksums were fixed up after damage reaches the frame walk: still a typed error, no panic and no
/// closed connection.
#[test]
fn a_damaged_frame_with_good_checksums_is_refused_before_the_client_path() {
    let rec = record_session(
        "net_replay_forged",
        30,
        RecorderConfig {
            keyframe_every_s: 100.0,
            ..RecorderConfig::default()
        },
    );
    let bytes = std::fs::read(&rec.path).expect("read");
    let body = u32::from_le_bytes(bytes[12..16].try_into().expect("four")) as usize;
    let mut at = 20 + body;
    let mut forged = 0;
    let mut outcomes = BTreeMap::new();
    // Each tick chunk's frame gets garbage bytes and a repaired CRC.
    while at + 13 <= bytes.len() {
        let kind = bytes[at];
        let len = u32::from_le_bytes(bytes[at + 5..at + 9].try_into().expect("four")) as usize;
        if kind == 2 && len > 12 {
            let mut bad = bytes.clone();
            for b in &mut bad[at + 13 + 7..at + 13 + len] {
                *b = b.wrapping_mul(31).wrapping_add(0x5a);
            }
            let crc = {
                let mut head = [0u8; 9];
                head.copy_from_slice(&bad[at..at + 9]);
                let mut all = head.to_vec();
                all.extend_from_slice(&bad[at + 13..at + 13 + len]);
                regia_net::recorder::format::netdemo_crc32(&all)
            };
            bad[at + 9..at + 13].copy_from_slice(&crc.to_le_bytes());
            let path = temp_file("net_replay_forged_one");
            std::fs::write(&path, &bad).expect("write");
            let mut world = replay_world(setup);
            match NetReplayPlayer::open(&mut world, &path, plain_options()) {
                Err(e) => {
                    *outcomes.entry(e.code()).or_insert(0) += 1;
                }
                Ok(mut player) => {
                    for _ in rec.first..=rec.last {
                        replay_tick(&mut world, &mut player, 1.0);
                    }
                    let code = player.error().map_or("played", NetDemoError::code);
                    *outcomes.entry(code).or_insert(0) += 1;
                }
            }
            forged += 1;
            if forged >= 12 {
                break;
            }
        }
        at += 13 + len;
    }
    assert!(forged > 0);
    for code in outcomes.keys() {
        assert!(matches!(*code, "netdemo_corrupt" | "played"), "{outcomes:?}");
    }
    // Garbage that decodes as something is allowed to play. None refused would mean the walk is not there at all.
    assert!(
        outcomes.get("netdemo_corrupt").is_some_and(|n| *n > 0),
        "nothing was refused: {outcomes:?}"
    );
}

/// The reader's target of `aim_of`, as the copy's value says.
fn aimed_at(world: &World, id: NetId) -> Option<Entity> {
    let e = world.resource::<NetEntities>().entity(id)?;
    world.get::<Aim>(e).map(|a| a.target)
}

/// Spec 103 § 3.6: a ring's connection is older than the export, so it gains no copy at the export's first keyframe and
/// nothing heals what that keyframe holds. A reference to a copy later in its list still resolves on the first tick.
#[test]
fn a_first_keyframe_resolves_references_to_copies_later_in_its_list() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    start_ring(
        &mut hub.server.world,
        RingConfig {
            keyframe_every_s: 0.2,
            ..RingConfig::default()
        },
    )
    .expect("starts");
    // Each aims at the other, so whichever comes first in the keyframe's list holds a reference to a copy later in it.
    let aim = || Aim {
        target: Entity::PLACEHOLDER,
    };
    let one = hub.server.world.spawn((Replicated::default(), aim())).id();
    let other = hub.server.world.spawn((Replicated::default(), aim())).id();
    hub.server.world.get_mut::<Aim>(one).expect("aim").target = other;
    hub.server.world.get_mut::<Aim>(other).expect("aim").target = one;
    hub.run(60);
    let one_id = *hub.server.world.get::<NetId>(one).expect("id");
    let other_id = *hub.server.world.get::<NetId>(other).expect("id");
    let path = temp_file("net_replay_first_key_refs");
    let summary =
        regia_net::recorder::export_ring(&mut hub.server.world, &path, Some(0.0), false).expect("exports");
    assert!(
        summary.first_tick.0 > 20,
        "past the spawn's own frames: {summary:?}"
    );
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &path, plain_options()).expect("opens");
    replay_tick(&mut world, &mut player, 1.0);
    assert_eq!(player.error(), None);
    for (from, to) in [(one_id, other_id), (other_id, one_id)] {
        let shown = aimed_at(&world, from).expect("the copy");
        assert_eq!(
            world.resource::<NetEntities>().net_id(shown),
            Some(to),
            "{from:?} resolved on the first tick"
        );
    }
}

/// Spec 103 § 3.6: a seek's reset forgets every copy, and nothing later in a quiet stretch sends a reference again. The
/// keyframe's two passes resolve it, whichever copy comes first in its list.
#[test]
fn a_seek_resolves_references_to_copies_later_in_its_keyframe() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let path = temp_file("net_replay_seek_refs");
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            keyframe_every_s: 0.2,
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    let aim = || Aim {
        target: Entity::PLACEHOLDER,
    };
    let one = hub.server.world.spawn((Replicated::default(), aim())).id();
    let other = hub.server.world.spawn((Replicated::default(), aim())).id();
    hub.server.world.get_mut::<Aim>(one).expect("aim").target = other;
    hub.server.world.get_mut::<Aim>(other).expect("aim").target = one;
    hub.run(90);
    let one_id = *hub.server.world.get::<NetId>(one).expect("id");
    let other_id = *hub.server.world.get::<NetId>(other).expect("id");
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    assert!(summary.keyframes >= 3, "{summary:?}");
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &path, plain_options()).expect("opens");
    for _ in summary.first_tick.0..=summary.last_tick.0 {
        replay_tick(&mut world, &mut player, 1.0);
    }
    // Backward, well past the spawn's own frames: the reset path.
    player
        .seek(summary.first_tick.0 + (summary.last_tick.0 - summary.first_tick.0) / 2)
        .expect("in range");
    replay_tick(&mut world, &mut player, 0.0);
    assert_eq!(player.error(), None);
    assert!(!player.state(&world).seeking);
    for (from, to) in [(one_id, other_id), (other_id, one_id)] {
        let shown = aimed_at(&world, from).expect("the copy");
        assert_eq!(
            world.resource::<NetEntities>().net_id(shown),
            Some(to),
            "{from:?} resolved after a seek"
        );
    }
}

/// Spec 103 § 3.6: in a player's view, a target that leaves the view still exists on the server, so the referrer has to
/// be named for it. Playback then holds no entity that is gone.
#[test]
fn a_target_leaving_a_players_view_clears_the_reference() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    install_world_profile(&mut hub.server.world);
    let path = temp_file("net_replay_view_refs");
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            view: RecorderView::Player(pa),
            keyframe_every_s: 100.0,
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    let target = hub
        .server
        .world
        .spawn((
            Replicated::default(),
            Health::default(),
            world_profile(),
            NetVisibility {
                allow: vec![pa],
                deny: Vec::new(),
            },
        ))
        .id();
    let referrer = hub
        .server
        .world
        .spawn((Replicated::default(), Aim { target }, world_profile()))
        .id();
    hub.run(10);
    let referrer_id = *hub.server.world.get::<NetId>(referrer).expect("id");
    let mut visibility = hub
        .server
        .world
        .get_mut::<NetVisibility>(target)
        .expect("visibility");
    visibility.allow.clear();
    visibility.deny = vec![pa];
    hub.run(90);
    assert!(
        hub.server.world.get::<Health>(target).is_some(),
        "it still exists"
    );
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &path, plain_options()).expect("opens");
    for _ in summary.first_tick.0..=summary.last_tick.0 {
        replay_tick(&mut world, &mut player, 1.0);
    }
    let mut q = world.query::<&Health>();
    assert_eq!(q.iter(&world).count(), 0, "the target left the view");
    assert_eq!(
        aimed_at(&world, referrer_id),
        Some(Entity::PLACEHOLDER),
        "no reference to a copy that is gone"
    );
}

fn relevant_to_peer_one(_: &World, _: Entity, peer: PeerId) -> bool {
    peer == PeerId(1)
}

/// Spec 103 § 3.3: without the relevance pipeline a player's view is the game's filter, and it freezes the same way.
#[test]
fn a_departed_players_view_freezes_without_the_pipeline_too() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let a = hub.add_client();
    let pa = hub.clients[a].peer;
    assert_eq!(pa, PeerId(1));
    hub.server.world.insert_resource(NetRelevance {
        filter: Some(relevant_to_peer_one),
    });
    let path = temp_file("net_replay_view_gone_filter");
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            view: RecorderView::Player(pa),
            keyframe_every_s: 0.1,
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    let held = hub
        .server
        .world
        .spawn((Replicated::default(), Health::default()))
        .id();
    hub.run(5);
    regia_net::runtime::server_remove_peer(&mut hub.server.world, pa);
    hub.run(2);
    assert!(regia_net::recorder::recorder_status(&hub.server.world).view_peer_gone);
    hub.server.world.get_mut::<Health>(held).expect("health").hp = 42;
    // The filter would admit it for that peer forever: the frozen view keeps it out.
    hub.server.world.spawn((Replicated::default(), Health::default()));
    hub.run(30);
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &path, plain_options()).expect("opens");
    for _ in summary.first_tick.0..=summary.last_tick.0 {
        replay_tick(&mut world, &mut player, 1.0);
    }
    let mut q = world.query::<&Health>();
    let values: Vec<i32> = q.iter(&world).map(|h| h.hp).collect();
    assert_eq!(values, vec![42], "what it held updated, the newcomer stayed out");
    // A keyframe's view froze with it: a seek shows the same.
    player.seek(summary.last_tick.0).expect("in range");
    replay_tick(&mut world, &mut player, 0.0);
    let mut q = world.query::<&Health>();
    let values: Vec<i32> = q.iter(&world).map(|h| h.hp).collect();
    assert_eq!(values, vec![42], "after a seek");
}

/// Spec 103 § 8: after the end, `play` starts over as the open did: the same state at each tick, and the first tick's
/// messages again, which a seek would have kept back.
#[test]
fn starting_over_after_the_end_plays_as_the_first_time() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    hub.run(2);
    let path = temp_file("net_replay_restart");
    start_recording(&mut hub.server.world, &path, RecorderConfig::default()).expect("starts");
    hub.server
        .world
        .run_system_once(|mut net: NetSender| {
            let _ = net.broadcast_session(Boom { power: 7 });
        })
        .expect("system runs");
    hub.server.world.spawn((Replicated::default(), Health::default()));
    hub.run(20);
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &path, plain_options()).expect("opens");
    let pass = |world: &mut World, player: &mut NetReplayPlayer| {
        let mut seen = Vec::new();
        for _ in summary.first_tick.0..=summary.last_tick.0 {
            replay_tick(world, player, 1.0);
            seen.push((digest(world), messages_of(world)));
        }
        seen
    };
    let first = pass(&mut world, &mut player);
    assert!(player.state(&world).ended);
    assert!(
        first.iter().any(|(_, m)| !m.is_empty()),
        "the recording's first tick has a message"
    );
    player.play();
    assert!(!player.state(&world).ended);
    let again = pass(&mut world, &mut player);
    assert_eq!(first, again);
    assert_eq!(player.state(&world).generation, 1);
    assert_eq!(player.error(), None);
}

/// Spec 103 § 8: a session started while a replay is open owns the world: the player stops, leaves its traffic alone, and
/// its close takes neither entities nor mode.
#[test]
fn a_session_started_during_a_replay_takes_over() {
    let rec = record_session("net_replay_takeover", 10, RecorderConfig::default());
    let mut world = replay_world(setup);
    let mut player = NetReplayPlayer::open(&mut world, &rec.path, plain_options()).expect("opens");
    replay_tick(&mut world, &mut player, 1.0);
    let mut session = NetSession::default();
    session
        .host_local(regia_net::session::lifecycle::HostOptions::default(), true)
        .expect("hosts");
    world.insert_resource(session);
    world.insert_resource(NetMode::DedicatedServer);
    let theirs = world
        .spawn((
            Replicated::default(),
            Health {
                hp: 5,
                ..Health::default()
            },
        ))
        .id();
    player.advance_ticks(&mut world, 1.0);
    assert_eq!(
        player.error().map(NetDemoError::code),
        Some("netdemo_session_active")
    );
    assert!(!player.state(&world).playing);
    world
        .resource_mut::<NetNode>()
        .outbox
        .push(regia_net::runtime::Datagram {
            peer: PeerId(9),
            channel: regia_net::message::Channel::ReliableOrdered,
            bytes: vec![1, 2, 3],
        });
    player.after_send(&mut world);
    assert_eq!(
        world.resource::<NetNode>().outbox.len(),
        1,
        "a session's traffic stays"
    );
    player.close(&mut world);
    assert!(world.get::<Health>(theirs).is_some(), "its entities stay");
    assert_eq!(
        *world.resource::<NetMode>(),
        NetMode::DedicatedServer,
        "and its mode"
    );
}

/// A running recorder is a server's, in whatever mode: opening a replay there would take what it records.
#[test]
fn a_server_or_a_recording_world_is_refused() {
    let rec = record_session("net_replay_server", 10, RecorderConfig::default());
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let kept = hub
        .server
        .world
        .spawn((Replicated::default(), Health::default()))
        .id();
    let err = NetReplayPlayer::open(&mut hub.server.world, &rec.path, plain_options()).expect_err("a server");
    assert_eq!(err.code(), "netdemo_session_active");
    assert!(hub.server.world.get::<Health>(kept).is_some());

    let mut hub = LoopbackHub::new(NetMode::Offline, 5, setup);
    start_ring(&mut hub.server.world, RingConfig::default()).expect("starts");
    let err =
        NetReplayPlayer::open(&mut hub.server.world, &rec.path, plain_options()).expect_err("recording");
    assert_eq!(err.code(), "netdemo_session_active");
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Blob {
    #[replicate(max_len = 1100000)]
    data: String,
}

fn setup_blobs(world: &mut World) {
    register_replicated::<Blob>(world, ReplicateOptions::default());
    register_replicated::<Aim>(world, ReplicateOptions::default());
}

/// Spec 103 § 3.6: a keyframe over half the client's pending limit cannot go twice at once. Both passes arrive, first
/// feed and seek alike, and nothing reads as damage.
#[test]
fn a_keyframe_too_large_to_feed_twice_at_once_still_goes_twice() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup_blobs);
    hub.run(2);
    let path = temp_file("net_replay_large_key");
    start_recording(&mut hub.server.world, &path, RecorderConfig::default()).expect("starts");
    let filler = "x".repeat(1 << 20);
    for _ in 0..34 {
        hub.server
            .world
            .spawn((Replicated::default(), Blob { data: filler.clone() }));
    }
    hub.run(3);
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    assert!(summary.bytes > 34 << 20, "{summary:?}");
    let mut world = replay_world(setup_blobs);
    let mut player = NetReplayPlayer::open(&mut world, &path, plain_options()).expect("opens");
    let blobs = |world: &mut World| world.query::<&Blob>().iter(world).count();
    for _ in 0..16 {
        replay_tick(&mut world, &mut player, 1.0);
    }
    assert_eq!(player.error(), None);
    assert_eq!(blobs(&mut world), 34, "linear play");
    player.seek(summary.last_tick.0).expect("in range");
    for _ in 0..8 {
        replay_tick(&mut world, &mut player, 0.0);
    }
    assert!(!player.state(&world).seeking);
    assert_eq!(player.error(), None);
    assert_eq!(blobs(&mut world), 34, "after a seek");
}

fn interp_world() -> World {
    let mut w = replay_world(setup);
    w.resource_mut::<NetSettings>().interpolation.min_delay_ms = 50.0;
    w.resource_mut::<NetSettings>().interpolation.max_delay_ms = 50.0;
    w
}

/// Spec 103 § 12.11: with the replay clock a proxy is shown a delay in the past, hidden until its spawn is shown, kept
/// until its despawn is shown; pause freezes it, and speed moves it faster.
#[test]
fn interpolation_runs_on_playback_time() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let path = temp_file("net_replay_interp");
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            keyframe_every_s: 0.5,
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    let mover = hub
        .server
        .world
        .spawn((
            Replicated {
                transform: TransformReplication::Movement,
                ..Replicated::default()
            },
            Transform::default(),
            Health::default(),
        ))
        .id();
    hub.step();
    let spawned_at = hub.server_tick().0 - 1;
    for i in 1..=120 {
        hub.server
            .world
            .get_mut::<Transform>(mover)
            .expect("tr")
            .translation
            .x = i as f32;
        hub.step();
    }
    let _ = hub.server.world.despawn(mover);
    let despawn_tick = hub.server_tick().0;
    hub.run(30);
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    assert_eq!(summary.first_tick.0, spawned_at);

    let mut world = interp_world();
    let mut player = NetReplayPlayer::open(&mut world, &path, ReplayOptions::default()).expect("opens");
    assert!(world.contains_resource::<NetInterpolation>());
    assert_eq!(
        world.resource::<NetInterpolation>().default_profile.interpolation,
        InterpolationMode::Linear
    );
    let x = |world: &mut World| {
        let mut q = world.query_filtered::<&Transform, With<NetId>>();
        q.iter(world).next().map(|t| t.translation.x)
    };
    let frame = |world: &mut World, player: &mut NetReplayPlayer, ticks: f64| {
        replay_tick(world, player, ticks);
        player.interpolate(world);
    };
    // 60 ticks in: x equals the tick less the 50 ms (3 ticks) delay.
    for _ in 0..60 {
        frame(&mut world, &mut player, 1.0);
    }
    let shown = x(&mut world).expect("the copy");
    let now = (player.state(&world).tick - spawned_at) as f32;
    let behind = now - shown;
    assert!(
        (2.5..=4.5).contains(&behind),
        "shown {behind} ticks behind (at {shown}, now {now})"
    );
    // Pause: nothing moves, no starvation counted.
    player.pause();
    let starved = world.resource::<NetInterpolation>().counters.frames_starved;
    for _ in 0..30 {
        frame(&mut world, &mut player, 1.0);
    }
    assert_eq!(x(&mut world), Some(shown), "paused");
    assert_eq!(
        world.resource::<NetInterpolation>().counters.frames_starved,
        starved,
        "pause starves nothing"
    );
    // Double speed: two ticks of playback per step.
    player.play();
    for _ in 0..10 {
        frame(&mut world, &mut player, 2.0);
    }
    let moved = x(&mut world).expect("the copy") - shown;
    assert!((16.0..=24.0).contains(&moved), "moved {moved} in 10 steps at 2x");
    // The despawn waits for the delay, then the copy goes.
    while player.state(&world).tick < despawn_tick {
        frame(&mut world, &mut player, 1.0);
    }
    assert!(
        x(&mut world).is_some(),
        "still shown: its despawn is 3 ticks ahead of the view"
    );
    for _ in 0..6 {
        frame(&mut world, &mut player, 1.0);
    }
    assert_eq!(x(&mut world), None, "gone once its despawn tick is shown");
    // A seek back brings it back, shown at the target less the delay after the clock settles.
    player.seek(spawned_at + 60).expect("in range");
    player.play();
    frame(&mut world, &mut player, 0.0);
    for _ in 0..10 {
        frame(&mut world, &mut player, 1.0);
    }
    let shown = x(&mut world).expect("back");
    let now = (player.state(&world).tick - spawned_at) as f32;
    assert!(
        (2.0..=5.0).contains(&(now - shown)),
        "after a seek: {shown} at {now}"
    );
}

/// The whole app path: `NetPlugin`, `NetReplayPlugin`, fixed stepping and the open/close calls the agent uses.
#[test]
fn the_plugin_plays_in_an_app_and_the_camera_follows_through_a_seek() {
    let rec = record_session(
        "net_replay_app",
        120,
        RecorderConfig {
            keyframe_every_s: 0.5,
            ..RecorderConfig::default()
        },
    );
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins(NetPlugin {
        mode: NetMode::Client,
    });
    {
        let mut settings = app.world_mut().resource_mut::<NetSettings>();
        settings.replication.state_channel = StateChannel::Reliable;
        settings.replication.send_every_ticks = 1;
    }
    setup(app.world_mut());
    app.add_plugins((SpectatorPlugin, NetReplayPlugin));
    app.update();
    assert_eq!(replay_state(app.world()), None, "inert until something opens");
    open_replay(app.world_mut(), &rec.path, plain_options()).expect("opens");
    assert_eq!(app.world().resource::<LocalPeer>().0, Some(RECORDER_PEER));
    for _ in 0..30 {
        app.update();
    }
    let state = replay_state(app.world()).expect("open");
    assert!(state.playing);
    assert!(state.tick >= rec.first + 25, "played: {state:?}");
    assert_eq!(
        app.world().resource::<NetNode>().outbox.len(),
        0,
        "nothing queues up"
    );
    // Follow the first copy, by id.
    let id = app
        .world()
        .resource::<NetEntities>()
        .iter()
        .next()
        .map(|(i, _)| i)
        .expect("a copy");
    app.world_mut()
        .resource_mut::<NetReplayPlayer>()
        .set_camera(ReplayCamera::Follow(id));
    app.update();
    let followed = app
        .world()
        .resource::<SpectatorDrive>()
        .follow
        .expect("following");
    assert_eq!(app.world().resource::<NetEntities>().net_id(followed), Some(id));
    assert_eq!(
        replay_state(app.world()).expect("open").follow_entity,
        Some(followed)
    );
    // Backward: the world resets and the copy is made again.
    app.world_mut()
        .resource_mut::<NetReplayPlayer>()
        .seek(rec.first + 10)
        .expect("in range");
    app.update();
    app.update();
    let refollowed = app
        .world()
        .resource::<SpectatorDrive>()
        .follow
        .expect("following");
    assert_eq!(app.world().resource::<NetEntities>().net_id(refollowed), Some(id));
    assert_ne!(
        refollowed, followed,
        "a seek made a new copy, and the follow moved to it"
    );
    // Pause holds the tick, speed scales it.
    app.world_mut().resource_mut::<NetReplayPlayer>().pause();
    let held = replay_state(app.world()).expect("open").tick;
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(replay_state(app.world()).expect("open").tick, held);
    {
        let mut player = app.world_mut().resource_mut::<NetReplayPlayer>();
        player.play();
        player.set_speed(4.0);
    }
    for _ in 0..5 {
        app.update();
    }
    let ran = replay_state(app.world()).expect("open").tick - held;
    assert!((15..=25).contains(&ran), "4x for 5 frames ran {ran} ticks");
    for _ in 0..200 {
        app.update();
    }
    let state = replay_state(app.world()).expect("open");
    assert!(state.ended, "{state:?}");
    assert_eq!(state.tick, rec.last);
    close_replay(app.world_mut());
    assert_eq!(replay_state(app.world()), None);
    assert!(
        app.world().resource::<NetEntities>().is_empty(),
        "closing removes the copies"
    );
}

thread_local! {
    static RESTORES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn count_restore(_world: &mut World, _level: &str) {
    RESTORES.with(|c| c.set(c.get() + 1));
}

/// ADR-8762: a second open over a running replay is refused with `replay_already_open`, so the
/// one close returns the world to its own NetMode and LocalPeer and runs the level restore once.
#[test]
fn a_second_open_over_a_running_replay_is_refused_and_close_restores_once() {
    let rec = record_session("net_replay_twice", 10, RecorderConfig::default());
    let mut world = replay_world(setup);
    world.insert_resource(LocalPeer(Some(PeerId(42))));
    RESTORES.with(|c| c.set(0));
    let options = ReplayOptions {
        level_restore: Some(count_restore),
        ..plain_options()
    };
    let first = NetReplayPlayer::open(&mut world, &rec.path, options).expect("opens");
    world.insert_resource(first);
    let second = NetReplayPlayer::open(&mut world, &rec.path, options);
    assert_eq!(
        second.as_ref().err().map(NetDemoError::code),
        Some("replay_already_open"),
        "a second open over a running replay is refused"
    );
    if let Ok(second) = second {
        // The base: the second player saved the replay's own mode; it replaces the first.
        world.insert_resource(second);
    }
    close_replay(&mut world);
    assert_eq!(*world.resource::<NetMode>(), NetMode::Client);
    assert_eq!(
        world.resource::<LocalPeer>().0,
        Some(PeerId(42)),
        "the world's own local peer comes back"
    );
    assert_eq!(
        RESTORES.with(std::cell::Cell::get),
        1,
        "the level restore runs once"
    );
    // Once closed, a new open is accepted again.
    let again = NetReplayPlayer::open(&mut world, &rec.path, options).expect("opens after the close");
    again.close(&mut world);
    assert_eq!(world.resource::<LocalPeer>().0, Some(PeerId(42)));
}

/// ADR-8762: a player dropped without its close leaves the world in replay mode; the next open
/// still restores the mode from before the first replay.
#[test]
fn an_open_after_a_dropped_player_restores_the_original_mode() {
    let rec = record_session("net_replay_dropped", 10, RecorderConfig::default());
    let mut world = replay_world(setup);
    world.insert_resource(LocalPeer(Some(PeerId(42))));
    let dropped = NetReplayPlayer::open(&mut world, &rec.path, plain_options()).expect("opens");
    drop(dropped);
    let player =
        NetReplayPlayer::open(&mut world, &rec.path, plain_options()).expect("a dropped player is not open");
    player.close(&mut world);
    assert_eq!(*world.resource::<NetMode>(), NetMode::Client);
    assert_eq!(world.resource::<LocalPeer>().0, Some(PeerId(42)));
}

/// ADR-8762: `open_replay` replaces the running replay: it closes the old player (its restore
/// runs) before it opens, and the final close still restores the world's own mode.
#[test]
fn open_replay_twice_closes_the_first_and_restores_the_original_mode() {
    let rec = record_session("net_replay_replace", 10, RecorderConfig::default());
    let mut world = replay_world(setup);
    world.insert_resource(LocalPeer(Some(PeerId(42))));
    RESTORES.with(|c| c.set(0));
    let options = ReplayOptions {
        level_restore: Some(count_restore),
        ..plain_options()
    };
    open_replay(&mut world, &rec.path, options).expect("opens");
    open_replay(&mut world, &rec.path, options).expect("replaces");
    assert_eq!(
        RESTORES.with(std::cell::Cell::get),
        1,
        "the replaced player closed"
    );
    close_replay(&mut world);
    assert_eq!(RESTORES.with(std::cell::Cell::get), 2);
    assert_eq!(world.resource::<LocalPeer>().0, Some(PeerId(42)));
    assert!(world.get_resource::<NetReplayPlayer>().is_none());
}

/// ADR-8850: a dead mark (a player dropped without its close) is reused only while the world is
/// still that dropped replay's (`LocalPeer == RECORDER_PEER`). A real session that changed the
/// mode since owns the world by then, and the next replay's close restores the session's mode,
/// not the mode from before the drop (rev-claude-55 finding 4).
#[test]
fn a_replay_after_a_dropped_player_and_a_ended_session_restores_the_sessions_mode() {
    let rec = record_session("net_replay_dropped_session", 10, RecorderConfig::default());
    let mut world = replay_world(setup);
    // The world's own mode before any replay: offline, its own peer.
    world.insert_resource(NetMode::Offline);
    world.insert_resource(LocalPeer(Some(PeerId(42))));
    let dropped = NetReplayPlayer::open(&mut world, &rec.path, plain_options()).expect("opens");
    drop(dropped);
    // A real session took the world (a join: client mode, its own peer), then ended; its mode
    // and local peer are what the world kept.
    let mut session = NetSession::default();
    session
        .host_local(regia_net::session::lifecycle::HostOptions::default(), true)
        .expect("hosts");
    world.insert_resource(session);
    world.insert_resource(NetMode::Client);
    world.insert_resource(LocalPeer(Some(PeerId(9))));
    world.resource_mut::<NetSession>().reset();
    let player =
        NetReplayPlayer::open(&mut world, &rec.path, plain_options()).expect("a dead mark is not open");
    player.close(&mut world);
    assert_eq!(
        *world.resource::<NetMode>(),
        NetMode::Client,
        "the mode after the close is the real session's, not the pre-drop offline mode"
    );
    assert_eq!(
        world.resource::<LocalPeer>().0,
        Some(PeerId(9)),
        "and the session's local peer"
    );
}
