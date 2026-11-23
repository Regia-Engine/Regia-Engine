// SPDX-License-Identifier: MPL-2.0
//! Client capture and replay of an ECS world through `NetLink` (spec 120 § 15.5, ADR-8221,
//! ADR-8890): a client of the time harness ([`TimedHub`], the real transport over the impaired
//! network in virtual time) predicts its own hero and interpolates the other players' heroes and
//! a strafing NPC on a bad-wifi link. Its `NetLink` captures the session; a fresh world replays the
//! capture through `NetLink::replay` and must write the same receive and flush times, the same end
//! and the same chained frame digest (prediction corrections, predicted state, interpolated
//! proxies) after every update.
//!
//! Seeded, virtual time, no wall clock, no sleeps.
// The time harness runs ECS worlds (feature `ecs`, on by default).
#![cfg(feature = "ecs")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_ecs::prelude::*;
use regia_net::bridge::NetFrame;
use regia_net::capture::{CaptureFile, CaptureRecord, CaptureTap, ReplayFeed};
use regia_net::interp::{NetInterpolation, install_interpolation, net_interpolate};
use regia_net::predict::ClientPrediction;
use regia_net::replication::{NetPredicted, register_replicated};
use regia_net::runtime::NetNode;
use regia_net::testing::NetProcess;
use regia_net::*;
use regia_net_soak::profiles::Profile;
use regia_net_soak::timed::{FrameDigest, TimedHub, TimedOptions, TimedReplay, replay_client};
use regia_net_soak::workload::Fnv64;
use regia_scene::transform::Transform;

// ---- the test game: the time harness's hero, plus interpolated proxies --------------------------

struct Game;
impl NetGame for Game {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.link_capture_replay";
    const PROTOCOL_VERSION: u32 = 1;
}

/// Another game, for the refused replay.
struct OtherGame;
impl NetGame for OtherGame {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.link_capture_replay.other";
    const PROTOCOL_VERSION: u32 = 1;
}

#[derive(Clone, Debug, PartialEq, Default, Replicate)]
struct Pad {
    x: i8,
    jump: bool,
}

impl NetInput for Pad {
    fn on_missing(last: &Self) -> Self {
        Self {
            jump: false,
            ..last.clone()
        }
    }
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Motion {
    vy: f32,
    grounded: bool,
}

/// The server's NPC: it strafes, so every client interpolates a moving proxy.
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Strafer;

type Inputs<'w, 's> = Query<'w, 's, (&'static NetPlayer, &'static InputCommand<Pad>)>;
type HeroData = (
    &'static mut Transform,
    &'static mut Motion,
    Option<&'static NetOwner>,
    Option<&'static NetRole>,
);

fn step_hero(cx: PredictionCx, inputs: Inputs, mut heroes: Query<HeroData>) {
    for (mut t, mut m, owner, role) in &mut heroes {
        if !cx.simulates(role) {
            continue;
        }
        let player = cx.owner_player(owner);
        let pad = inputs
            .iter()
            .find(|(p, _)| Some(p.id) == player)
            .map(|(_, c)| c.value.clone())
            .unwrap_or_default();
        t.translation.x += f32::from(pad.x) * 0.1;
        if pad.jump && m.grounded {
            m.vy = 5.0;
            m.grounded = false;
        }
        if !m.grounded {
            m.vy -= 9.8 / 60.0;
            t.translation.y += m.vy / 60.0;
            if t.translation.y <= 0.0 {
                t.translation.y = 0.0;
                m.vy = 0.0;
                m.grounded = true;
            }
        }
    }
}

fn setup(world: &mut World) {
    register_replicated::<Motion>(world, ReplicateOptions::default());
    register_replicated::<Strafer>(world, ReplicateOptions::default());
    add_net_input::<Pad>(world).expect("one input type");
    predict::<Transform>(world, Tolerance::position(0.01)).expect("transform");
    predict::<Motion>(world, Tolerance::Exact).expect("motion");
    add_predicted_systems(world, step_hero);
    if !world.resource::<NetMode>().is_server() {
        let every = world.resource::<NetSettings>().replication.send_every_ticks;
        install_interpolation(world, 60.0, f64::from(every.max(1)));
    }
}

/// Client gameplay: shows the proxies at the link clock, then stamps the next tick's input.
fn client_frame(world: &mut World) {
    if let Some(now) = world.get_resource::<NetInterpolation>().map(|n| n.now_ms) {
        net_interpolate(world, now);
    }
    let Some(next) = world
        .get_resource::<ClientPrediction>()
        .map(|cp| cp.client_tick().0 + 1)
    else {
        return;
    };
    let pad = Pad {
        x: if (next / 37).is_multiple_of(3) { -1 } else { 1 },
        jump: next.is_multiple_of(50),
    };
    let mut q = world.query::<&mut InputCommand<Pad>>();
    for mut c in q.iter_mut(world) {
        c.value = pad.clone();
    }
}

fn server_strafe(world: &mut World) {
    let tick = world.resource::<NetNode>().tick.0;
    let mut q = world.query_filtered::<&mut Transform, With<Strafer>>();
    for mut t in q.iter_mut(world) {
        t.translation.x = ((tick % 240) as f32 / 240.0 * std::f32::consts::TAU).sin() * 6.0;
    }
}

/// The server knocks every hero aside every 1.5 s: a push the owning client cannot predict, so its
/// prediction corrects (spec 73).
fn server_knockback(world: &mut World) {
    let tick = world.resource::<NetNode>().tick.0;
    if tick == 0 || !tick.is_multiple_of(90) {
        return;
    }
    let mut q = world.query_filtered::<&mut Transform, With<Motion>>();
    for mut t in q.iter_mut(world) {
        t.translation.z += 0.5;
    }
}

fn configure_client(proc: &mut NetProcess) {
    proc.gameplay.add_systems(client_frame);
}

/// One entity in [`world_digest`]: its `NetId`, position bits, motion (`vy` bits, grounded) and role.
type DigestRow = (u32, [u32; 3], Option<(u32, bool)>, u8);

/// What a client shows and predicts after an update: every replicated entity by `NetId` (its
/// position bits, its motion, its role), the prediction counters and the interpolation counters.
fn world_digest(world: &mut World) -> u64 {
    let mut rows: Vec<DigestRow> = world
        .query::<(&NetId, &Transform, Option<&Motion>, Option<&NetRole>)>()
        .iter(world)
        .map(|(id, t, m, r)| {
            let p = t.translation;
            (
                id.0,
                [p.x.to_bits(), p.y.to_bits(), p.z.to_bits()],
                m.map(|m| (m.vy.to_bits(), m.grounded)),
                match r {
                    None => 0,
                    Some(NetRole::Local) => 1,
                    Some(NetRole::Authority) => 2,
                    Some(NetRole::Predicted) => 3,
                    Some(NetRole::Proxy) => 4,
                },
            )
        })
        .collect();
    rows.sort_by_key(|r| r.0);
    let mut h = Fnv64::new(0);
    h.u64(rows.len() as u64);
    for (id, p, m, role) in rows {
        h.u64(u64::from(id)).u64(u64::from(role));
        for b in p {
            h.u64(u64::from(b));
        }
        if let Some((vy, g)) = m {
            h.u64(u64::from(vy)).u64(u64::from(g));
        }
    }
    if let Some(cp) = world.get_resource::<ClientPrediction>() {
        let s = &cp.stats;
        h.u64(s.corrections)
            .u64(s.resets)
            .u64(u64::from(cp.client_tick().0));
    }
    if let Some(ni) = world.get_resource::<NetInterpolation>() {
        let c = &ni.counters;
        h.u64(c.frames_starved)
            .u64(c.frames_extrapolated)
            .u64(c.recoveries)
            .u64(c.snaps);
    }
    h.finish()
}

const DIGEST: FrameDigest = world_digest;

/// The bad-wifi profile of spec 120 § 15.1 for client `i`.
fn bad_wifi(i: usize, duration_ms: u64) -> regia_net_soak::impair::LinkSpec {
    Profile::named("bad-wifi")
        .expect("bad-wifi profile")
        .client_link(i, duration_ms)
}

/// A live run: `clients` clients on bad-wifi, each with a hero it predicts, the server's
/// strafing NPC, for `play_ms` of virtual time after every client connected; client 0 is
/// captured into a ring of `ring_bytes`.
fn live(seed: u64, clients: usize, play_ms: u64, ring_bytes: usize) -> (TimedHub, CaptureTap) {
    let options = TimedOptions {
        seed,
        ..TimedOptions::default()
    };
    let mut hub = TimedHub::new::<Game>(options, setup).expect("server");
    hub.server()
        .gameplay
        .add_systems((server_strafe, server_knockback));
    let mut tap = None;
    for i in 0..clients {
        let c = hub
            .add_client::<Game>(bad_wifi(i, play_ms + 10_000))
            .expect("client");
        configure_client(hub.client(c));
        if i == 0 {
            tap = Some(hub.capture_client(c, ring_bytes, DIGEST).expect("capture"));
        }
    }
    let n = hub.client_count();
    assert!(
        hub.run_until(10_000, |h| (0..n).all(|c| h.client_peer(c).is_some())),
        "every client connects"
    );
    for c in 0..n {
        let peer = hub.client_peer(c).expect("peer");
        hub.server().world.spawn((
            Replicated {
                transform: TransformReplication::PositionOnly,
                ..Replicated::default()
            },
            Transform::default(),
            Motion {
                vy: 0.0,
                grounded: true,
            },
            NetPredicted,
            NetOwner(peer),
        ));
    }
    hub.server().world.spawn((
        Replicated {
            transform: TransformReplication::PositionOnly,
            ..Replicated::default()
        },
        Transform::default(),
        Strafer,
    ));
    hub.run_ms(play_ms);
    (hub, tap.expect("client 0 captured"))
}

fn only_session(tap: &CaptureTap) -> Vec<CaptureRecord> {
    let mut sessions = tap.lock().sessions();
    assert_eq!(sessions.len(), 1, "one connection, one session");
    sessions.remove(0)
}

fn replay(session: &[CaptureRecord]) -> TimedReplay {
    replay_client::<Game>(session, setup, configure_client, DIGEST).expect("replay")
}

fn count(session: &[CaptureRecord], pred: impl Fn(&CaptureRecord) -> bool) -> usize {
    session.iter().filter(|r| pred(r)).count()
}

/// Acceptance (ADR-8890): a captured bad-wifi session replays through `NetLink::replay` to the
/// same digest after every update, with the same prediction corrections, at the captured times.
#[test]
fn a_captured_bad_wifi_session_replays_through_netlink_to_the_same_digest() {
    let (mut hub, tap) = live(0x5EED_0890, 3, 20_000, 16 << 20);
    let session = only_session(&tap);
    let live_digest = hub.client_digest(0).expect("captured client");
    let live_stats = hub
        .client_world(0)
        .get_resource::<ClientPrediction>()
        .map(|cp| cp.stats.clone())
        .expect("client 0 predicts");
    let frames = count(&session, |r| matches!(r, CaptureRecord::Frame { .. }));
    let packets = count(&session, |r| matches!(r, CaptureRecord::Packet { .. }));
    let proxies = {
        let w = &mut hub.client(0).world;
        w.query::<&NetRole>()
            .iter(w)
            .filter(|r| **r == NetRole::Proxy)
            .count()
    };
    let r = replay(&session);
    println!(
        "link-capture-replay: frames {frames}, packets {packets}, proxies {proxies}, corrections live {} replay {:?}, \
         resets live {}, digest live {live_digest:#018x} replay {:#018x}, progress {:?}, divergence {:?}",
        live_stats.corrections,
        r.prediction.as_ref().map(|s| s.corrections),
        live_stats.resets,
        r.digest,
        r.progress,
        r.divergence,
    );
    assert!(frames > 1_000, "the session ran: {frames} frame markers");
    assert!(packets > 500, "the session received: {packets} packets");
    assert!(
        proxies >= 3,
        "the client interpolates the two other heroes and the NPC: {proxies}"
    );
    assert!(
        live_stats.corrections > 0,
        "the knockback makes the client correct its prediction: {live_stats:?}"
    );
    assert_eq!(r.divergence, None, "the replay writes what the capture holds");
    assert_eq!(
        r.captured_digest,
        Some(live_digest),
        "the capture's last frame is the live digest"
    );
    assert_eq!(r.digest, live_digest, "same chained digest");
    assert!(r.matches());
    let replayed = r.prediction.expect("the replay predicts");
    assert_eq!(replayed.corrections, live_stats.corrections, "same corrections");
    assert_eq!(replayed.resets, live_stats.resets, "same resets");
    let progress = r.progress.expect("a replay link");
    assert_eq!(progress.time_mismatches, 0, "every receive at its captured time");
    assert_eq!(progress.overruns, 0, "no receive past the capture");
    assert_eq!(
        r.updates as usize,
        count(&session, |r| matches!(r, CaptureRecord::Receive { .. }))
    );
}

/// A replay depends on the captured bytes: one changed byte in one captured datagram changes the
/// digest (the comparison names a frame marker).
#[test]
fn a_one_byte_change_in_a_captured_packet_changes_the_replay() {
    let (_hub, tap) = live(0x5EED_0891, 2, 8_000, 16 << 20);
    let mut session = only_session(&tap);
    assert!(replay(&session).matches(), "the unchanged capture replays");
    // The largest packet in the second half: a state update the client applies.
    let half = session.len() / 2;
    let (at, _) = session
        .iter()
        .enumerate()
        .skip(half)
        .filter_map(|(i, r)| match r {
            CaptureRecord::Packet { bytes } => Some((i, bytes.len())),
            _ => None,
        })
        .max_by_key(|(i, len)| (*len, std::cmp::Reverse(*i)))
        .expect("a packet in the second half");
    if let CaptureRecord::Packet { bytes } = &mut session[at] {
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x5a;
    }
    let r = replay(&session);
    println!(
        "link-capture-replay one-byte: record {at}, divergence {:?}",
        r.divergence
    );
    assert!(!r.matches(), "a changed byte changes what the client shows");
    assert!(
        r.divergence.is_some(),
        "the comparison names the first difference"
    );
}

static DIRS: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "regia-link-capture-{tag}-{}-{}",
        std::process::id(),
        DIRS.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// The saved `.netcapture` replays the same as the ring: the file round trip keeps every record.
#[test]
fn a_saved_capture_replays_from_the_file() {
    let (hub, tap) = live(0x5EED_0892, 2, 5_000, 16 << 20);
    let dir = temp_dir("file");
    let path = dir.join("client-0.netcapture");
    let saved = tap.save(&path, "link capture test", false).expect("save");
    assert!(saved.bytes > 0 && saved.sessions == 1);
    let file = CaptureFile::load(&path).expect("load");
    assert_eq!(file.label, "link capture test");
    let sessions = file.sessions();
    let r = replay(&sessions[0]);
    assert!(r.matches(), "divergence {:?}", r.divergence);
    assert_eq!(Some(r.digest), hub.client_digest(0));
    // A second save to the same path is refused, and leaves the file whole.
    let again = tap.save(&path, "again", false).expect_err("exists");
    assert_eq!(again.code(), "capture_exists");
    assert_eq!(
        CaptureFile::load(&path).expect("still whole").label,
        "link capture test"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `NetLink::replay` refuses a capture of another game with `capture_protocol_mismatch`, and a
/// server link takes no capture.
#[test]
fn another_game_is_refused_and_a_server_takes_no_capture() {
    let (mut hub, tap) = live(0x5EED_0893, 1, 2_000, 1 << 20);
    let session = only_session(&tap);
    let err = replay_client::<OtherGame>(&session, setup, configure_client, DIGEST).expect_err("refused");
    let regia_net_soak::timed::TimedError::Link(link) = err else {
        panic!("a link error: {err}");
    };
    assert_eq!(link.code(), "capture_protocol_mismatch");
    let mut server_res = hub.server().world.resource_mut::<NetLink>();
    let server_link = &mut *server_res;
    assert!(!server_link.set_capture(Some(CaptureTap::new(1 << 20).expect("tap"))));
    assert!(server_link.capture().is_none());
    // A running link refuses a replay on top of it.
    let feed = ReplayFeed::new(session).expect("feed");
    let busy = server_link
        .replay::<Game>(feed, &LinkOptions::default())
        .expect_err("running");
    assert_eq!(busy.code(), "link_running");
}

/// A `NetLink` capture stays within its ring on a bad-wifi run longer than the ring holds: it
/// evicts the oldest records and never grows.
#[test]
fn a_small_ring_stays_within_its_bytes() {
    let ring = 64 << 10;
    let (_hub, tap) = live(0x5EED_0894, 2, 15_000, ring);
    let stats = tap.stats();
    println!("link-capture-replay ring: {stats:?}");
    assert_eq!(stats.ring_bytes, ring);
    assert!(stats.peak_used_bytes <= ring, "never past the ring: {stats:?}");
    assert!(stats.evicted > 0, "a 15 s session does not fit 64 KiB: {stats:?}");
    assert!(
        tap.lock().heap_bytes() <= ring + (4 << 10),
        "the ring is the heap: {stats:?}"
    );
}
