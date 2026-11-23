// SPDX-License-Identifier: MPL-2.0
//! The recorder connection and the `.netdemo` container (spec 103, ADR-3021): it changes nothing for real clients,
//! files are atomic and capped, the ring stays in its bound, and a damaged file is a typed error, never a panic.
//! Playback is tested in `regia_framework/tests/net_replay.rs`.

use std::io::Cursor;
use std::path::PathBuf;

use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_net::message::{NetSender, register_net_message};
use regia_net::model::{NetId, NetMode, NetOwner, PeerId};
use regia_net::recorder::format::{ChunkKind, NETDEMO_MAGIC, frames_of};
use regia_net::recorder::{
    EndReason, NetDemoError, NetDemoReader, RECORDER_PEER, RecorderConfig, RecorderView, RingConfig,
    export_ring, is_virtual_peer, recorder_status, start_recording, start_ring, stop_recording, stop_ring,
};
use regia_net::replication::{
    NetSleep, ReplicateOptions, Replicated, ServerReplication, register_replicated,
};
use regia_net::runtime::NetNode;
use regia_net::settings::{NetSettings, StateChannel};
use regia_net::testing::{LoopbackHub, PacketLog};
use regia_net::*;

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Health {
    hp: i32,
    #[replicate(max_len = 64)]
    label: String,
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

fn setup(world: &mut World) {
    register_replicated::<Health>(world, ReplicateOptions::default());
    register_net_message::<Boom>(world);
    register_net_message::<Spark>(world);
}

fn setup_state_channel(world: &mut World) {
    world.resource_mut::<NetSettings>().replication.state_channel = StateChannel::Unreliable;
    setup(world);
}

/// A fresh directory: pid and an atomic counter, so parallel tests never share one.
fn unique_dir(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn temp(name: &str) -> PathBuf {
    unique_dir(name).join("match.netdemo")
}

fn spawn_health(hub: &mut LoopbackHub, hp: i32) -> Entity {
    hub.server
        .world
        .spawn((
            Replicated::default(),
            Health {
                hp,
                label: format!("unit {hp}"),
            },
        ))
        .id()
}

/// A scripted session: spawns, changes, despawns, ownership changes, reliable and unreliable messages, an entity that
/// falls asleep.
fn play_script(hub: &mut LoopbackHub, ticks: u32) {
    let owner = hub.clients.first().map_or(PeerId::SERVER, |c| c.peer);
    let mut alive: Vec<Entity> = Vec::new();
    let sleeper = hub
        .server
        .world
        .spawn((
            Replicated {
                sleep: NetSleep::AfterQuiet { quiet_ticks: 5 },
                ..Replicated::default()
            },
            Health {
                hp: 1,
                label: "door".to_string(),
            },
        ))
        .id();
    for t in 0..ticks {
        if t.is_multiple_of(7) {
            alive.push(spawn_health(hub, t as i32));
        }
        if t % 11 == 5
            && let Some(e) = alive.first().copied()
        {
            let _ = hub.server.world.despawn(e);
            alive.remove(0);
        }
        if t % 13 == 3
            && let Some(e) = alive.last().copied()
        {
            hub.server.world.entity_mut(e).insert(NetOwner(owner));
        }
        if t % 17 == 4
            && let Some(e) = alive.last().copied()
        {
            hub.server.world.entity_mut(e).insert(NetOwner(PeerId::SERVER));
        }
        for (i, e) in alive.iter().enumerate() {
            if (t as usize + i).is_multiple_of(3)
                && let Some(mut h) = hub.server.world.get_mut::<Health>(*e)
            {
                h.hp += 1;
            }
        }
        if t == 40
            && let Some(mut h) = hub.server.world.get_mut::<Health>(sleeper)
        {
            h.hp = 2;
        }
        if t > 3 && t.is_multiple_of(5) {
            let target = alive
                .last()
                .copied()
                .filter(|e| hub.server.world.get::<NetId>(*e).is_some());
            hub.server
                .world
                .run_system_once(move |mut net: NetSender| {
                    if let Some(e) = target {
                        let _ = net.broadcast(e, Boom { power: t });
                        let _ = net.broadcast(e, Spark { n: t });
                    }
                    let _ = net.broadcast_session(Boom { power: t + 1000 });
                })
                .expect("system runs");
        }
        hub.step();
    }
}

fn run_session(setup_fn: fn(&mut World), interest: bool, record: Option<&PathBuf>) -> Vec<PacketLog> {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup_fn);
    hub.negotiate = true;
    hub.add_client();
    hub.add_client();
    if interest {
        regia_net::relevance::install_net_interest(&mut hub.server.world);
    }
    if let Some(path) = record {
        start_recording(
            &mut hub.server.world,
            path,
            RecorderConfig {
                keyframe_every_s: 0.1,
                ..RecorderConfig::default()
            },
        )
        .expect("starts");
    }
    play_script(&mut hub, 200);
    if record.is_some() {
        stop_recording(&mut hub.server.world).expect("stops");
    }
    hub.client_logs
}

/// Spec 103 § 3.4: real clients receive the same bytes with a recorder and without one, on either wire, with and
/// without the relevance pipeline, across keyframes (one every 6 ticks here), sleep and unreliable messages.
#[test]
fn a_recorder_changes_no_byte_a_client_receives() {
    for (name, setup_fn, interest) in [
        ("stream", setup as fn(&mut World), false),
        ("stream+interest", setup, true),
        ("state channel", setup_state_channel, false),
        ("state channel+interest", setup_state_channel, true),
    ] {
        let path = temp("recorder_transparency");
        let without = run_session(setup_fn, interest, None);
        let with = run_session(setup_fn, interest, Some(&path));
        assert_eq!(without.len(), with.len());
        for (client, (a, b)) in without.iter().zip(&with).enumerate() {
            assert!(!a.is_empty(), "{name}: client {client} got frames");
            assert_eq!(a.len(), b.len(), "{name}: client {client}'s frame count");
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                assert_eq!(x, y, "{name}: client {client}, frame {i}");
            }
        }
    }
}

/// How a recorder comes and goes in [`run_with_recorder`].
#[derive(Clone, Copy, Debug)]
enum Rig {
    /// Nothing recording: the control.
    None,
    /// A file and a ring on one connection, started and stopped mid-session.
    FileAndRing,
    /// A ring alone, for the session's whole length.
    Ring,
    /// A file on a player's view, started and stopped mid-session.
    PlayerView,
}

fn run_with_recorder(rig: Rig, interest: bool, rate: u32) -> Vec<PacketLog> {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    hub.negotiate = true;
    hub.server
        .world
        .resource_mut::<NetSettings>()
        .replication
        .server_rate_bytes = rate;
    hub.add_client();
    hub.add_client();
    if interest {
        regia_net::relevance::install_net_interest(&mut hub.server.world);
    }
    let path = temp("recorder_transparency_rig");
    let ring = RingConfig {
        seconds: 1.0,
        max_bytes: 1 << 20,
        keyframe_every_s: 0.1,
        ..RingConfig::default()
    };
    if matches!(rig, Rig::Ring) {
        start_ring(&mut hub.server.world, ring).expect("starts");
    }
    play_script(&mut hub, 60);
    let file = RecorderConfig {
        keyframe_every_s: 0.1,
        ..RecorderConfig::default()
    };
    match rig {
        Rig::None | Rig::Ring => {}
        Rig::FileAndRing => {
            start_recording(&mut hub.server.world, &path, file).expect("starts");
            start_ring(&mut hub.server.world, ring).expect("starts");
        }
        Rig::PlayerView => {
            let view = RecorderView::Player(PeerId(1));
            let config = RecorderConfig { view, ..file };
            start_recording(&mut hub.server.world, &path, config).expect("starts");
        }
    }
    play_script(&mut hub, 80);
    match rig {
        Rig::None | Rig::Ring => {}
        Rig::FileAndRing => {
            stop_recording(&mut hub.server.world).expect("stops");
            stop_ring(&mut hub.server.world).expect("stops");
        }
        Rig::PlayerView => {
            stop_recording(&mut hub.server.world).expect("stops");
        }
    }
    play_script(&mut hub, 60);
    hub.client_logs
}

/// Spec 103 § 3.4, the rest of it: a ring, a player's view, a recorder that starts and stops mid-session, and the
/// server-wide rate limit, which sums what the connections send and must not count a recorder's.
#[test]
fn a_ring_a_player_view_and_a_mid_session_start_change_no_byte_either() {
    for (interest, rate) in [(false, 0), (true, 0), (true, 300)] {
        let without = run_with_recorder(Rig::None, interest, rate);
        for rig in [Rig::FileAndRing, Rig::Ring, Rig::PlayerView] {
            let with = run_with_recorder(rig, interest, rate);
            assert_eq!(without.len(), with.len());
            for (client, (a, b)) in without.iter().zip(&with).enumerate() {
                assert!(!a.is_empty(), "{rig:?}: client {client} got frames");
                assert_eq!(
                    a.len(),
                    b.len(),
                    "{rig:?} {interest} {rate}: client {client}'s frame count"
                );
                for (i, (x, y)) in a.iter().zip(b).enumerate() {
                    assert_eq!(x, y, "{rig:?} {interest} {rate}: client {client}, frame {i}");
                }
            }
        }
    }
}

/// Spec 103 § 3.1: what a game asks of the peer list and of an entity's holders names real peers, and a recorder's
/// connection, which never acknowledges, does not count as waiting.
#[test]
fn the_peer_apis_do_not_name_the_connection() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let path = temp("recorder_peer_apis");
    start_recording(&mut hub.server.world, &path, RecorderConfig::default()).expect("starts");
    let e = spawn_health(&mut hub, 1);
    hub.run(4);
    let id = *hub.server.world.get::<NetId>(e).expect("id");
    let rep = hub.server.world.resource::<ServerReplication>();
    assert!(rep.conn_holds(RECORDER_PEER, id));
    assert_eq!(rep.holders(id), Vec::<PeerId>::new());
    assert_eq!(rep.pending_acks(id), 0, "nobody is waited for");
    assert!(!hub.server.world.resource::<NetNode>().has_peer(RECORDER_PEER));
    let refused = start_ring(
        &mut hub.server.world,
        RingConfig {
            view: RecorderView::Player(RECORDER_PEER),
            ..RingConfig::default()
        },
    );
    assert_eq!(refused.expect_err("not a peer").code(), "recorder_view_conflict");
    stop_recording(&mut hub.server.world).expect("stops");
    let refused = start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            view: RecorderView::Player(RECORDER_PEER),
            overwrite: true,
            ..RecorderConfig::default()
        },
    );
    assert_eq!(refused.expect_err("not a peer").code(), "recorder_view_invalid");
}

fn hosted(hub: &mut LoopbackHub, level: &str) {
    let mut session = regia_net::NetSession::default();
    session
        .host_local(
            regia_net::session::lifecycle::HostOptions {
                level: level.to_string(),
                ..Default::default()
            },
            true,
        )
        .expect("hosts");
    hub.server.world.insert_resource(session);
}

fn travel(hub: &mut LoopbackHub, level: &str) {
    hub.server
        .world
        .resource_mut::<regia_net::NetSession>()
        .travel(level)
        .expect("travels");
}

/// Spec 103 § 6: `level` is the header's label. A label unlike the session's level ends nothing, and the session moving
/// ends the file, whatever the label says.
#[test]
fn a_label_ends_nothing_and_travel_ends_the_file() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    hosted(&mut hub, "map1");
    let path = temp("recorder_label");
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            level: "the finals".to_string(),
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    spawn_health(&mut hub, 1);
    hub.run(10);
    let status = recorder_status(&hub.server.world);
    assert_eq!(status.file.as_ref().and_then(|f| f.ended), None, "{status:?}");
    assert!(status.ticks >= 9);
    travel(&mut hub, "map2");
    hub.run(3);
    let status = recorder_status(&hub.server.world);
    assert_eq!(
        status.file.as_ref().and_then(|f| f.ended),
        Some(EndReason::LevelChanged),
        "{status:?}"
    );
    let summary = stop_recording(&mut hub.server.world).expect("collects");
    assert_eq!(summary.reason, EndReason::LevelChanged);
    let reader = NetDemoReader::open(std::fs::File::open(&path).expect("open")).expect("a valid file");
    assert_eq!(reader.header().level, "the finals");
}

/// A session hosted after the recording began names its level for the first time: that is no travel.
#[test]
fn a_session_hosted_later_ends_nothing() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let path = temp("recorder_host_later");
    start_recording(&mut hub.server.world, &path, RecorderConfig::default()).expect("starts");
    spawn_health(&mut hub, 1);
    hub.run(4);
    hosted(&mut hub, "map1");
    hub.run(4);
    let status = recorder_status(&hub.server.world);
    assert_eq!(status.file.as_ref().and_then(|f| f.ended), None, "{status:?}");
    travel(&mut hub, "map2");
    hub.run(3);
    assert_eq!(
        stop_recording(&mut hub.server.world).expect("collects").reason,
        EndReason::LevelChanged
    );
}

/// Spec 103 § 7: a ring runs across travel, and starts over, so an export holds one level and names it.
#[test]
fn travel_starts_the_ring_over() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    hosted(&mut hub, "map1");
    let w = &mut hub.server.world;
    start_ring(
        w,
        RingConfig {
            seconds: 60.0,
            keyframe_every_s: 0.1,
            ..RingConfig::default()
        },
    )
    .expect("starts");
    spawn_health(&mut hub, 1);
    hub.run(20);
    let before = recorder_status(&hub.server.world).ring.expect("a ring");
    travel(&mut hub, "map2");
    let travelled_at = hub.server_tick().0;
    hub.run(20);
    let after = recorder_status(&hub.server.world).ring.expect("a ring");
    assert!(
        after.first_tick.expect("held").0 > travelled_at,
        "{before:?} then {after:?}, travel at {travelled_at}"
    );
    let path = temp("recorder_ring_travel");
    export_ring(&mut hub.server.world, &path, None, false).expect("exports");
    let reader = NetDemoReader::open(std::fs::File::open(&path).expect("open")).expect("a valid file");
    assert_eq!(reader.header().level, "map2");
}

/// Spec 103 § 6: a clean exit drops the world (ADR-188), and that keeps the recording.
#[test]
fn dropping_the_world_keeps_the_recording() {
    let path = temp("recorder_drop");
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    start_recording(&mut hub.server.world, &path, RecorderConfig::default()).expect("starts");
    spawn_health(&mut hub, 1);
    hub.run(10);
    assert!(!path.exists());
    drop(hub);
    let reader = NetDemoReader::open(std::fs::File::open(&path).expect("the file")).expect("a valid file");
    assert_eq!(reader.end().reason, EndReason::Stopped);
    let dir = path.parent().expect("a parent");
    assert_eq!(
        std::fs::read_dir(dir).expect("dir").count(),
        1,
        "no tmp file left"
    );

    // One a cap had already ended, closing when the world went.
    let path = temp("recorder_drop_cap");
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            max_seconds: 0.05,
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    spawn_health(&mut hub, 1);
    hub.run(30);
    let ended = recorder_status(&hub.server.world).file.and_then(|f| f.ended);
    assert_eq!(ended, Some(EndReason::TimeCap));
    drop(hub);
    let reader = NetDemoReader::open(std::fs::File::open(&path).expect("the file")).expect("a valid file");
    assert_eq!(reader.end().reason, EndReason::TimeCap);

    // Nothing recorded: nothing to keep, and nothing left behind.
    let path = temp("recorder_drop_empty");
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    start_recording(&mut hub.server.world, &path, RecorderConfig::default()).expect("starts");
    drop(hub);
    let dir = path.parent().expect("a parent");
    assert_eq!(std::fs::read_dir(dir).expect("dir").count(), 0);
}

/// Spec 103 § 6: an export to a recording's own path does not disturb the recording, and is not replaced by it either.
#[test]
fn an_export_to_a_recordings_path_harms_neither() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let path = temp("recorder_same_path");
    start_recording(&mut hub.server.world, &path, RecorderConfig::default()).expect("starts");
    start_ring(&mut hub.server.world, RingConfig::default()).expect("starts");
    spawn_health(&mut hub, 1);
    hub.run(10);
    let exported = export_ring(&mut hub.server.world, &path, None, false).expect("exports");
    hub.run(10);
    let err = stop_recording(&mut hub.server.world).expect_err("a file appeared");
    assert_eq!(err.code(), "netdemo_exists");
    assert_eq!(
        std::fs::metadata(&path).expect("the export").len(),
        exported.bytes,
        "the export is untouched"
    );
    NetDemoReader::open(std::fs::File::open(&path).expect("open")).expect("a valid export");
    let dir = path.parent().expect("a parent");
    let tmp: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "tmp"))
        .collect();
    assert_eq!(tmp.len(), 1, "{tmp:?}");
    assert!(err.to_string().contains(&*tmp[0].to_string_lossy()), "{err}");
    let reader =
        NetDemoReader::open(std::fs::File::open(&tmp[0]).expect("open")).expect("the recording is whole");
    assert_eq!(reader.end().reason, EndReason::Stopped);
}

/// Spec 103 § 7: a file's shorter keyframe interval leaves the ring's window whole.
#[test]
fn a_files_shorter_keyframe_interval_does_not_cut_the_ring_short() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let hz = regia_net::runtime::tick_hz(&hub.server.world);
    let path = temp("recorder_ring_interval");
    start_ring(
        &mut hub.server.world,
        RingConfig {
            seconds: 4.0,
            keyframe_every_s: 1.0,
            ..RingConfig::default()
        },
    )
    .expect("starts");
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            keyframe_every_s: 0.1,
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    spawn_health(&mut hub, 1);
    hub.run((hz * 12.0) as usize);
    let ring = recorder_status(&hub.server.world).ring.expect("a ring");
    let span = f64::from(ring.last_tick.expect("held").0 - ring.first_tick.expect("held").0);
    assert!(span >= 4.0 * hz, "{span} ticks at {hz} Hz: {ring:?}");
    assert!(ring.segments <= 7, "{ring:?}");
    assert_eq!(ring.overflows, 0, "{ring:?}");
    let cut = temp("recorder_ring_interval_cut");
    let summary = export_ring(&mut hub.server.world, &cut, Some(4.0), false).expect("exports");
    assert!(
        f64::from(summary.last_tick.0 - summary.first_tick.0) >= 4.0 * hz,
        "{summary:?}"
    );
    stop_recording(&mut hub.server.world).expect("stops");
}

/// Spec 103 § 5: a tick over the chunk limit is counted, and the file stays valid whether or not another tick follows.
#[test]
fn a_tick_over_the_chunk_limit_is_counted_and_the_file_stays_valid() {
    use regia_net::message::Channel;
    use regia_net::model::NetTick;
    use regia_net::recorder::NetRecorder;
    let huge = vec![0u8; regia_net::recorder::format::MAX_CHUNK_BYTES + 1];
    let oversize: &[(Channel, &[u8])] = &[(Channel::ReliableOrdered, huge.as_slice())];
    // A real, empty `TICK` frame: the container checks a frame's tick against its chunk's.
    let frame = |tick: u32| {
        let mut w = regia_net::bits::BitWriter::new();
        w.write_bits(1, 8);
        w.write_varint(u64::from(tick));
        for _ in 0..6 {
            w.write_varint(0);
        }
        w.finish()
    };
    let (ten, eleven, twelve) = (frame(10), frame(11), frame(12));
    for (another_follows, with_keyframe) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
        let path = temp("recorder_oversize");
        start_recording(&mut hub.server.world, &path, RecorderConfig::default()).expect("starts");
        {
            let mut rec = hub.server.world.resource_mut::<NetRecorder>();
            rec.record_frames(
                NetTick(10),
                Some(ten.as_slice()),
                &[(Channel::ReliableOrdered, ten.as_slice())],
            );
            // A keyframe on that tick is stored, and the file's last tick is still one with a tick chunk.
            let key = with_keyframe.then_some(eleven.as_slice());
            rec.record_frames(NetTick(11), key, oversize);
            if another_follows {
                rec.record_frames(
                    NetTick(12),
                    None,
                    &[(Channel::ReliableOrdered, twelve.as_slice())],
                );
            }
        }
        assert_eq!(recorder_status(&hub.server.world).ticks_skipped, 1);
        assert_eq!(
            recorder_status(&hub.server.world).file.map(|f| f.keyframes),
            Some(1 + u64::from(with_keyframe))
        );
        let summary = stop_recording(&mut hub.server.world).expect("stops");
        let last = if another_follows { 12 } else { 10 };
        assert_eq!(summary.last_tick, NetTick(last));
        let reader = NetDemoReader::open(std::fs::File::open(&path).expect("open")).expect("a valid file");
        assert_eq!(reader.last_tick(), NetTick(last));
    }
}

/// A name past the header's limit is cut on a character's edge: a cut inside one would make the file unreadable.
#[test]
fn a_long_label_is_cut_on_a_character() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let path = temp("recorder_long_label");
    let label = "\u{65e5}\u{672c}".repeat(40_000);
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            level: label.clone(),
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    spawn_health(&mut hub, 1);
    hub.run(3);
    stop_recording(&mut hub.server.world).expect("stops");
    let reader = NetDemoReader::open(std::fs::File::open(&path).expect("open")).expect("a readable file");
    let level = &reader.header().level;
    assert!(
        !level.is_empty() && label.starts_with(level.as_str()),
        "{}",
        level.len()
    );
}

/// The fuzz target's way past the checksums (spec 103 § 12.6): a resealed file with a damaged frame opens, so it reaches
/// the walk over its frames, which must then refuse it; and resealing a good file changes nothing.
#[test]
fn a_resealed_file_gets_past_the_checksums_to_the_frames() {
    use regia_net::recorder::format::{read_whole, resealed};
    let file = sample_file(30);
    assert_eq!(resealed(&file), file, "a good file is left as it is");
    let body_len = u32::from_le_bytes([file[12], file[13], file[14], file[15]]) as usize;
    let chunk_at = 20 + body_len;
    let len = u32::from_le_bytes(file[chunk_at + 5..chunk_at + 9].try_into().expect("four")) as usize;
    let mut bad = file.clone();
    // The keyframe's payload: garbage after its tag and its tick (one byte this early), which the container reads.
    for b in &mut bad[chunk_at + 13 + 2..chunk_at + 13 + len] {
        *b = b.wrapping_mul(31).wrapping_add(0x5a);
    }
    assert_eq!(
        read_whole(&bad).expect_err("the checksum stops it").code(),
        "netdemo_corrupt"
    );
    let sealed = resealed(&bad);
    let reader = NetDemoReader::open(Cursor::new(sealed.clone())).expect("the container now reads");
    assert!(reader.keyframes().len() > 1);
    assert_eq!(
        read_whole(&sealed).expect_err("the frame walk stops it").code(),
        "netdemo_corrupt"
    );
}

/// Spec 103 § 3.1: the session does not see it.
#[test]
fn the_connection_is_listed_nowhere_a_game_looks() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    hub.add_client();
    let path = temp("recorder_invisible");
    start_recording(&mut hub.server.world, &path, RecorderConfig::default()).expect("starts");
    let e = spawn_health(&mut hub, 1);
    hub.run(4);
    assert!(is_virtual_peer(RECORDER_PEER));
    let node = hub.server.world.resource::<NetNode>();
    assert_eq!(node.peers(), vec![PeerId(1)], "only the client");
    assert!(node.has_link(RECORDER_PEER), "the link is there");
    assert!(!node.has_peer(RECORDER_PEER), "and it is no peer of the game's");
    let id = *hub.server.world.get::<NetId>(e).expect("id");
    let rep = hub.server.world.resource::<ServerReplication>();
    assert_eq!(
        rep.holders(id),
        vec![PeerId(1)],
        "the game's list of holders names real peers"
    );
    assert!(rep.conn_holds(RECORDER_PEER, id), "it holds the copy");
    let queued = regia_net::replication::net_resend(&mut hub.server.world, e);
    assert_eq!(queued, vec![PeerId(1)], "net.resend names real peers");
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    assert!(summary.ticks >= 3);
    assert!(
        !hub.server.world.resource::<NetNode>().has_link(RECORDER_PEER),
        "stopping removes the connection"
    );
    assert!(
        !hub.server
            .world
            .resource::<ServerReplication>()
            .has_conn(RECORDER_PEER),
        "and its replication state"
    );
}

/// Spec 103 § 6: nothing appears at the path until the file is whole; the tmp file goes away.
#[test]
fn the_file_appears_only_when_it_is_whole() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let path = temp("recorder_atomic");
    let dir = path.parent().expect("dir").to_path_buf();
    start_recording(&mut hub.server.world, &path, RecorderConfig::default()).expect("starts");
    spawn_health(&mut hub, 1);
    hub.run(30);
    assert!(!path.exists(), "nothing at the path while recording");
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    assert_eq!(summary.reason, EndReason::Stopped);
    assert_eq!(std::fs::metadata(&path).expect("the file").len(), summary.bytes);
    let names: Vec<_> = std::fs::read_dir(&dir).expect("dir").flatten().collect();
    assert_eq!(names.len(), 1, "no tmp file left: {names:?}");
    let reader = NetDemoReader::open(std::fs::File::open(&path).expect("open")).expect("a valid file");
    assert_eq!(reader.end().reason, EndReason::Stopped);
    assert_eq!(reader.end().tick_chunks, summary.ticks);
    assert_eq!(reader.first_tick(), summary.first_tick);
    assert_eq!(reader.last_tick(), summary.last_tick);
}

#[test]
fn an_existing_file_is_refused_unless_overwriting() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let path = temp("recorder_exists");
    std::fs::write(&path, b"mine").expect("write");
    let err = start_recording(&mut hub.server.world, &path, RecorderConfig::default()).expect_err("refused");
    assert_eq!(err.code(), "netdemo_exists");
    assert_eq!(std::fs::read(&path).expect("read"), b"mine", "untouched");
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            overwrite: true,
            ..RecorderConfig::default()
        },
    )
    .expect("overwrite says yes");
    hub.run(3);
    stop_recording(&mut hub.server.world).expect("stops");
    assert_ne!(std::fs::read(&path).expect("read"), b"mine");
}

#[test]
fn a_directory_that_does_not_exist_is_an_io_error_at_the_start() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let path = temp("recorder_nodir").with_file_name("missing/match.netdemo");
    let err = start_recording(&mut hub.server.world, &path, RecorderConfig::default()).expect_err("refused");
    assert_eq!(err.code(), "netdemo_io");
    assert!(
        !hub.server.world.resource::<NetNode>().has_link(RECORDER_PEER),
        "no connection stays behind"
    );
}

#[test]
fn every_refusal_has_its_code() {
    let mut client = LoopbackHub::new(NetMode::Client, 5, setup);
    let path = temp("recorder_codes");
    assert_eq!(
        start_recording(&mut client.server.world, &path, RecorderConfig::default())
            .expect_err("a client")
            .code(),
        "net_not_server"
    );
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let w = &mut hub.server.world;
    assert_eq!(stop_recording(w).expect_err("idle").code(), "recorder_idle");
    assert_eq!(stop_ring(w).expect_err("idle").code(), "ring_idle");
    assert_eq!(
        export_ring(w, &path, None, false).expect_err("idle").code(),
        "ring_idle"
    );
    assert_eq!(
        start_ring(
            w,
            RingConfig {
                max_bytes: 1,
                ..RingConfig::default()
            }
        )
        .expect_err("small")
        .code(),
        "recorder_config_invalid"
    );
    assert_eq!(
        start_recording(
            w,
            &path,
            RecorderConfig {
                keyframe_every_s: 0.0,
                ..RecorderConfig::default()
            }
        )
        .expect_err("zero")
        .code(),
        "recorder_config_invalid"
    );
    assert_eq!(
        start_recording(
            w,
            &path,
            RecorderConfig {
                view: RecorderView::Player(PeerId(9)),
                ..RecorderConfig::default()
            }
        )
        .expect_err("not connected")
        .code(),
        "recorder_view_invalid"
    );
    assert!(!path.exists(), "a refused start left nothing");
    start_ring(w, RingConfig::default()).expect("starts");
    assert_eq!(
        start_ring(w, RingConfig::default()).expect_err("twice").code(),
        "recorder_busy"
    );
    assert_eq!(
        export_ring(w, &path, None, false)
            .expect_err("no tick yet")
            .code(),
        "ring_empty"
    );
    hub.add_client();
    let w = &mut hub.server.world;
    assert_eq!(
        start_recording(
            w,
            &path,
            RecorderConfig {
                view: RecorderView::Player(PeerId(1)),
                ..RecorderConfig::default()
            }
        )
        .expect_err("another view")
        .code(),
        "recorder_view_conflict"
    );
    start_recording(w, &path, RecorderConfig::default()).expect("the same view shares the connection");
    assert_eq!(
        start_recording(w, &path, RecorderConfig::default())
            .expect_err("twice")
            .code(),
        "recorder_busy"
    );
    hub.run(2);
    let w = &mut hub.server.world;
    stop_recording(w).expect("stops");
    assert!(
        w.resource::<NetNode>().has_link(RECORDER_PEER),
        "the ring keeps the connection"
    );
    stop_ring(w).expect("stops");
    assert!(!w.resource::<NetNode>().has_link(RECORDER_PEER));
}

/// Spec 103 § 6: a cap ends the recording cleanly: the file is valid and within the cap, and the recording stays
/// finished until collected.
#[test]
fn the_size_and_time_caps_end_with_a_valid_file_inside_the_cap() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let path = temp("recorder_cap_size");
    let cap = 8192u64;
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            max_bytes: cap,
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    for i in 0..600 {
        spawn_health(&mut hub, i);
        hub.step();
    }
    let status = recorder_status(&hub.server.world);
    assert_eq!(
        status.file.as_ref().and_then(|f| f.ended),
        Some(EndReason::SizeCap)
    );
    let summary = stop_recording(&mut hub.server.world).expect("collects");
    assert_eq!(summary.reason, EndReason::SizeCap);
    assert!(summary.bytes <= cap, "{} > {cap}", summary.bytes);
    assert_eq!(std::fs::metadata(&path).expect("file").len(), summary.bytes);
    let reader = NetDemoReader::open(std::fs::File::open(&path).expect("open")).expect("valid");
    assert_eq!(reader.end().reason, EndReason::SizeCap);

    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let path = temp("recorder_cap_time");
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            max_seconds: 1.0,
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    hub.run(300);
    let summary = stop_recording(&mut hub.server.world).expect("collects");
    assert_eq!(summary.reason, EndReason::TimeCap);
    // 60 Hz: the tick one second in ends it, before it is stored.
    assert_eq!(summary.last_tick.0 - summary.first_tick.0, 59);
    NetDemoReader::open(std::fs::File::open(&path).expect("open")).expect("valid");
}

/// Keyframes every N ticks; the first chunk is one; a late ring forces one at once.
#[test]
fn keyframes_come_on_their_interval() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let path = temp("recorder_keyframes");
    spawn_health(&mut hub, 1);
    hub.run(5);
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            keyframe_every_s: 0.5,
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    hub.run(121);
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    assert_eq!(summary.ticks, 121);
    let reader = NetDemoReader::open(std::fs::File::open(&path).expect("open")).expect("valid");
    let ticks: Vec<u32> = reader.keyframes().iter().map(|k| k.tick.0).collect();
    let first = reader.first_tick().0;
    assert_eq!(
        ticks,
        vec![first, first + 30, first + 60, first + 90, first + 120]
    );
}

/// Spec 103 § 7: bytes never pass the bound over a long session whose load changes; history is kept as far as bytes
/// allow; a burst larger than the ring counts an overflow and the ring recovers at the next keyframe.
#[test]
fn the_ring_stays_in_its_bound_over_a_long_session() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    let bound = 64 * 1024;
    start_ring(
        &mut hub.server.world,
        RingConfig {
            seconds: 2.0,
            max_bytes: bound,
            keyframe_every_s: 0.5,
            ..RingConfig::default()
        },
    )
    .expect("starts");
    let mut units: Vec<Entity> = (0..40).map(|i| spawn_health(&mut hub, i)).collect();
    let mut seen_window = 0;
    for t in 0..12_000u32 {
        // Three loads in turn: quiet, busy, and a burst of big labels over the bound.
        let phase = (t / 1000) % 3;
        match phase {
            0 => {
                if t.is_multiple_of(10)
                    && let Some(mut h) = hub.server.world.get_mut::<Health>(units[0])
                {
                    h.hp += 1;
                }
            }
            1 => {
                for e in units.clone() {
                    if let Some(mut h) = hub.server.world.get_mut::<Health>(e) {
                        h.hp += 1;
                    }
                }
            }
            _ => {
                if t % 1000 == 500 {
                    for i in 0..4000 {
                        units.push(spawn_health(&mut hub, i));
                    }
                }
                if t % 1000 == 700 {
                    for e in units.drain(40..) {
                        let _ = hub.server.world.despawn(e);
                    }
                }
            }
        }
        hub.step();
        let status = recorder_status(&hub.server.world);
        let ring = status.ring.expect("running");
        assert!(ring.used_bytes <= bound, "tick {t}: {} bytes", ring.used_bytes);
        assert!(ring.peak_bytes <= bound);
        if phase == 0
            && let (Some(a), Some(b)) = (ring.first_tick, ring.last_tick)
        {
            seen_window = seen_window.max(b.0 - a.0);
            assert!(
                b.0 - a.0 <= 2 * 60 + 30 + 1,
                "history past the window: {a:?}..{b:?}"
            );
        }
    }
    let ring = recorder_status(&hub.server.world).ring.expect("running");
    assert!(ring.overflows > 0, "the bursts overflowed it");
    assert!(ring.segments > 0, "and it recovered");
    assert!(
        seen_window >= 2 * 60,
        "it kept the whole window when bytes allowed: {seen_window}"
    );
    assert_eq!(ring.max_bytes, bound);
    let path = temp("recorder_ring_export");
    let summary = export_ring(&mut hub.server.world, &path, Some(1.0), false).expect("exports");
    let reader = NetDemoReader::open(std::fs::File::open(&path).expect("open")).expect("valid");
    assert_eq!(reader.end().tick_chunks, summary.ticks);
    assert_eq!(reader.last_tick(), ring.last_tick.expect("held"));
    assert!(
        reader.last_tick().0 - reader.first_tick().0 >= 60,
        "at least the second asked for"
    );
    stop_ring(&mut hub.server.world).expect("stops");
}

fn sample_file(ticks: u32) -> Vec<u8> {
    let path = temp("recorder_sample");
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
    hub.add_client();
    start_recording(
        &mut hub.server.world,
        &path,
        RecorderConfig {
            keyframe_every_s: 0.1,
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    play_script(&mut hub, ticks);
    stop_recording(&mut hub.server.world).expect("stops");
    std::fs::read(path).expect("read")
}

fn read_all(bytes: &[u8]) -> Result<u64, NetDemoError> {
    let mut reader = NetDemoReader::open(Cursor::new(bytes.to_vec()))?;
    let mut at = reader.first_chunk();
    let mut payload = Vec::new();
    let mut frames = 0;
    while at < reader.file_len() {
        let head = reader.read_chunk(at, &mut payload)?;
        if head.kind == ChunkKind::Tick {
            for frame in frames_of(&payload) {
                let (_, frame) = frame?;
                regia_net::recorder::playback::validate_frame(frame)?;
                frames += 1;
            }
        }
        at = head.next;
    }
    Ok(frames)
}

/// Spec 103 § 5: every cut length is `netdemo_truncated` (or `netdemo_not_a_demo` inside the magic), and every flipped bit
/// fails its checksum: none panics and none reads as valid.
#[test]
fn every_truncation_and_every_flipped_bit_is_a_typed_error() {
    let file = sample_file(30);
    assert!(read_all(&file).expect("the whole file reads") > 0);
    for cut in 0..file.len() {
        let err = read_all(&file[..cut]).expect_err("a cut file is refused");
        let want = if cut < NETDEMO_MAGIC.len() {
            "netdemo_not_a_demo"
        } else {
            "netdemo_truncated"
        };
        assert_eq!(err.code(), want, "cut at {cut}: {err}");
    }
    for bit in 0..file.len() * 8 {
        let mut bad = file.clone();
        bad[bit / 8] ^= 1 << (bit % 8);
        let err = read_all(&bad).expect_err("a flipped bit is refused");
        assert!(
            matches!(
                err.code(),
                "netdemo_not_a_demo" | "netdemo_version" | "netdemo_truncated" | "netdemo_corrupt"
            ),
            "bit {bit}: {err}"
        );
    }
    let mut longer = file.clone();
    longer.push(0);
    assert_eq!(read_all(&longer).expect_err("a tail").code(), "netdemo_corrupt");
    let mut future = file.clone();
    future[8..12].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(
        read_all(&future).expect_err("a newer file").code(),
        "netdemo_version"
    );
    assert_eq!(
        read_all(b"RIFFxxxxWEBPnot").expect_err("no").code(),
        "netdemo_not_a_demo"
    );
}

/// A length field lying about the size allocates nothing and is refused, past both checksums' reach.
#[test]
fn a_lying_length_is_refused() {
    let file = sample_file(10);
    let body_len = u32::from_le_bytes([file[12], file[13], file[14], file[15]]) as usize;
    let chunk_at = 20 + body_len;
    for (at, value) in [
        (12usize, u32::MAX),              // the header's length
        (chunk_at + 5, u32::MAX),         // a chunk over the limit
        (chunk_at + 5, 60 * 1024 * 1024), // a chunk under the limit, past the file
    ] {
        let mut bad = file.clone();
        bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
        let err = read_all(&bad).expect_err("refused");
        assert!(
            matches!(err.code(), "netdemo_corrupt" | "netdemo_truncated"),
            "{at}: {err}"
        );
    }
}
