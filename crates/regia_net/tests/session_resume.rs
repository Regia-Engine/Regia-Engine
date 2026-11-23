// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 10.3 (N102, ADR-8180): the session layer must not turn a recoverable hiccup into
//! a lost player. Each scenario runs under the soak harness's `good-wifi` and `flappy` numbers
//! (`book/agents/net-soak.md`), on an in-memory network with a virtual clock: seeded, no wall
//! time, no sleeps.
//!
//! - early return: a client that comes back from a new socket before the server's own timeout
//!   (a one-way upstream cut) resumes the same `PeerId` without a second slot;
//! - live-token takeover from `Loading`, `Joining`, `InGame` and interrupted (adversarial row 7);
//! - same-address rejoin of a quiet sealed peer enters grace (adversarial row 8);
//! - an authentic `Join` resend keeps a peer alive (adversarial row 9);
//! - slot reclaim on a full server (soak F6);
//! - streams survive a 5 s outage at 4x the default rate;
//! - a ban after a NAT rebind bans the new address.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use regia_net::client::{ClientConfig, ClientState, NetClient};
use regia_net::crypto::CryptoMode;
use regia_net::model::PeerId;
use regia_net::server::{NetServer, ServerConfig};
use regia_net::session::NetSession;
use regia_net::session::lifecycle::{
    ClientSessionEvent, ClientSessionState, HostOptions, LinkId, PeerState, ServerSessionEvent,
    SessionServer, SessionSettings,
};
use regia_net::session::manifest::{ContentManifest, ManifestAsset};
use regia_net::session::net_session::{ConnectOptions, SessionWire};
use regia_net::session::proto::{DisconnectReason, Hello, SessionMsg};
use regia_net::session::streams::StreamEvent;
use regia_net::socket::{DatagramSocket, LoopbackNet, LoopbackSocket};
use regia_net::{HealthPolicy, TimeoutPolicy};

/// Frame step of every scenario, milliseconds.
const STEP_MS: u64 = 16;

/// One access-link profile, one direction's numbers applied to both (the harness's `good-wifi`
/// uses the same latency, jitter, loss, reorder and duplicate rates up and down; its 20/50
/// Mbit/s caps are modeled as a serialization delay).
#[derive(Debug, Clone, Copy)]
struct Profile {
    name: &'static str,
    delay_ms: u64,
    jitter_ms: u64,
    loss: f64,
    reorder: f64,
    reorder_ms: u64,
    duplicate: f64,
    bandwidth_bps: u64,
    /// Short outages: first start, period, length (milliseconds of the virtual clock).
    flaps: Option<(u64, u64, u64)>,
}

/// `good-wifi` (regia_net_soak `profiles.rs`): 4 ms + 0-4 ms jitter, 0.2 % loss, 0.1 % reorder
/// by 10 ms, 0.05 % duplicates, 20 Mbit/s.
const GOOD_WIFI: Profile = Profile {
    name: "good-wifi",
    delay_ms: 4,
    jitter_ms: 4,
    loss: 0.002,
    reorder: 0.001,
    reorder_ms: 10,
    duplicate: 0.0005,
    bandwidth_bps: 20_000_000,
    flaps: None,
};

/// `flappy`: good-wifi plus a 3 s outage every 20 s from 15 s on.
const FLAPPY: Profile = Profile {
    name: "flappy",
    flaps: Some((15_000, 20_000, 3_000)),
    ..GOOD_WIFI
};

impl Profile {
    fn in_outage(&self, now: u64) -> bool {
        self.flaps
            .is_some_and(|(first, period, len)| now >= first && (now - first) % period < len)
    }
}

/// The virtual clock every socket and session of a scenario reads.
#[derive(Debug, Clone, Default)]
struct Clock(Arc<AtomicU64>);

impl Clock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    fn advance(&self, ms: u64) -> u64 {
        self.0.fetch_add(ms, Ordering::Relaxed) + ms
    }
}

/// Switches a test flips on a socket.
#[derive(Debug, Clone, Default)]
struct Switches {
    /// Drop everything this socket sends (a one-way cut of its upstream).
    mute: Arc<AtomicBool>,
    /// Drop everything this socket receives.
    deaf: Arc<AtomicBool>,
}

/// A [`LoopbackSocket`] behind a seeded impairment of its sends. The inner socket can be
/// replaced, which moves the socket to a new address (a NAT rebind).
struct Impaired {
    inner: Arc<Mutex<LoopbackSocket>>,
    clock: Clock,
    profile: Profile,
    rng: u64,
    held: Vec<(u64, Vec<u8>, SocketAddr)>,
    busy_until: u64,
    sw: Switches,
}

impl Impaired {
    fn new(socket: LoopbackSocket, clock: &Clock, profile: Profile, seed: u64) -> (Self, Switches) {
        let sw = Switches::default();
        (
            Self {
                inner: Arc::new(Mutex::new(socket)),
                clock: clock.clone(),
                profile,
                rng: seed ^ 0x9e37_79b9_7f4a_7c15,
                held: Vec::new(),
                busy_until: 0,
                sw: sw.clone(),
            },
            sw,
        )
    }

    /// SplitMix64.
    fn next(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn chance(&mut self, p: f64) -> bool {
        p > 0.0 && (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }

    fn release_due(&mut self) -> io::Result<()> {
        let now = self.clock.now();
        let mut inner = self.inner.lock().expect("socket");
        let mut i = 0;
        while i < self.held.len() {
            if self.held[i].0 <= now {
                let (_, bytes, dest) = self.held.swap_remove(i);
                inner.send_to(&bytes, dest)?;
            } else {
                i += 1;
            }
        }
        Ok(())
    }
}

impl DatagramSocket for Impaired {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        self.release_due()?;
        let now = self.clock.now();
        if self.sw.mute.load(Ordering::Relaxed)
            || self.profile.in_outage(now)
            || self.chance(self.profile.loss)
        {
            return Ok(());
        }
        // Serialization at the link rate, then latency, jitter and an occasional reorder hold.
        let wire_ms = (bytes.len() as u64 * 8 * 1000).div_ceil(self.profile.bandwidth_bps.max(1));
        self.busy_until = self.busy_until.max(now) + wire_ms;
        let jitter = self.next() % (self.profile.jitter_ms + 1);
        let reorder = if self.chance(self.profile.reorder) {
            self.profile.reorder_ms
        } else {
            0
        };
        let at = self.busy_until + self.profile.delay_ms + jitter + reorder;
        self.held.push((at, bytes.to_vec(), dest));
        if self.chance(self.profile.duplicate) {
            self.held.push((at + 1, bytes.to_vec(), dest));
        }
        Ok(())
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.release_due()?;
        let mut inner = self.inner.lock().expect("socket");
        loop {
            let got = inner.recv_from(buf)?;
            if got.is_some() && self.sw.deaf.load(Ordering::Relaxed) {
                continue;
            }
            return Ok(got);
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.lock().expect("socket").local_addr()
    }
}

fn manifest() -> ContentManifest {
    let base = vec![ManifestAsset {
        id: "levels/arena.level.ron".into(),
        kind: "level".into(),
        content_hash: 11,
    }];
    ContentManifest::build("0.5.0", "1.0.0", 1, 0xabc, &[("base", &base)])
}

fn host_opts() -> HostOptions {
    HostOptions {
        level: "levels/arena.level.ron".into(),
        ..HostOptions::default()
    }
}

/// One scenario: a server session and its clients on one virtual clock.
struct World {
    net: LoopbackNet,
    clock: Clock,
    profile: Profile,
    server: NetSession,
    server_addr: SocketAddr,
    server_events: Vec<ServerSessionEvent>,
    seed: u64,
}

impl World {
    fn new(profile: Profile, settings: SessionSettings, opts: HostOptions) -> Self {
        let net = LoopbackNet::new();
        let clock = Clock::default();
        let server_addr: SocketAddr = "127.0.0.1:7700".parse().expect("addr");
        let (socket, _) = Impaired::new(net.bind(server_addr).expect("bind"), &clock, profile, 1);
        let mut server = NetSession::new(settings, manifest());
        server.set_manual_clock(0);
        server.host_on(socket, opts, true).expect("host");
        Self {
            net,
            clock,
            profile,
            server,
            server_addr,
            server_events: Vec::new(),
            seed: 100,
        }
    }

    /// A client socket at `bind` (port 0 = a fresh port) with this scenario's profile.
    fn socket(&mut self, bind: &str) -> (Impaired, Switches) {
        self.seed += 1;
        let bound = self.net.bind(bind.parse().expect("addr")).expect("bind");
        Impaired::new(bound, &self.clock, self.profile, self.seed)
    }

    fn client(&mut self, settings: SessionSettings, opts: ConnectOptions) -> (NetSession, Switches) {
        let (socket, sw) = self.socket("127.0.0.1:0");
        let mut client = NetSession::new(settings, manifest());
        client.set_manual_clock(self.clock.now());
        client
            .connect_on(socket, self.server_addr, opts)
            .expect("connect");
        (client, sw)
    }

    /// One frame: every client, then the server.
    fn step(&mut self, clients: &mut [&mut NetSession]) {
        let now = self.clock.advance(STEP_MS);
        for c in clients.iter_mut() {
            c.set_manual_clock(now);
            c.update();
        }
        self.server.set_manual_clock(now);
        self.server.update();
        self.server_events.extend(self.server.take_server_events());
    }

    /// Steps until `done` or `limit_ms` of virtual time passed; true when `done` held.
    fn run_until(
        &mut self,
        clients: &mut [&mut NetSession],
        limit_ms: u64,
        mut done: impl FnMut(&World, &[&mut NetSession]) -> bool,
    ) -> bool {
        let end = self.clock.now() + limit_ms;
        while self.clock.now() < end {
            self.step(clients);
            if done(self, clients) {
                return true;
            }
        }
        false
    }

    fn run_for(&mut self, clients: &mut [&mut NetSession], ms: u64) {
        let end = self.clock.now() + ms;
        while self.clock.now() < end {
            self.step(clients);
        }
    }

    fn core(&self) -> &SessionServer {
        self.server.server().expect("server role")
    }
}

fn in_game(c: &NetSession) -> bool {
    c.state() == ClientSessionState::InGame
}

fn welcomed_peer(events: &[ClientSessionEvent]) -> Option<PeerId> {
    events.iter().rev().find_map(|e| match e {
        ClientSessionEvent::Welcomed(w) => Some(w.peer_id),
        _ => None,
    })
}

fn resumed(events: &[ServerSessionEvent], peer: PeerId) -> bool {
    events
        .iter()
        .any(|e| matches!(e, ServerSessionEvent::PlayerResumed { peer: p } if *p == peer))
}

fn left(events: &[ServerSessionEvent], peer: PeerId) -> Option<String> {
    events.iter().find_map(|e| match e {
        ServerSessionEvent::PlayerLeft { player, reason } if player.peer == peer => {
            Some(reason.code().to_owned())
        }
        _ => None,
    })
}

/// Joins one client and returns it in game with its `PeerId`.
fn joined(w: &mut World, settings: SessionSettings, opts: ConnectOptions) -> (NetSession, Switches, PeerId) {
    let (mut c, sw) = w.client(settings, opts);
    assert!(
        w.run_until(&mut [&mut c], 30_000, |_, cs| in_game(cs[0])),
        "{}: the client reached in_game ({:?})",
        w.profile.name,
        c.state()
    );
    // The client's InGame acknowledgement reaches the server.
    w.run_for(&mut [&mut c], 200);
    let peer = welcomed_peer(&c.take_client_events()).expect("welcomed");
    assert_eq!(
        w.core().peer_state(peer),
        Some(PeerState::InGame),
        "{}",
        w.profile.name
    );
    (c, sw, peer)
}

/// Plays until just before `flappy`'s first outage, so the hiccup under test meets it.
fn play_until_flap(w: &mut World, c: &mut NetSession) {
    let target = 13_000;
    if w.clock.now() < target {
        let ms = target - w.clock.now();
        w.run_for(&mut [c], ms);
    }
    assert!(in_game(c), "{}: still in game before the hiccup", w.profile.name);
}

/// The client gives up its old socket and reconnects from a new one with its token, the way a
/// game that retries at once does.
fn reconnect_with_token(w: &mut World, c: &mut NetSession, bind: &str) -> Switches {
    c.reset();
    let (socket, sw) = w.socket(bind);
    c.set_manual_clock(w.clock.now());
    c.connect_on(
        socket,
        w.server_addr,
        ConnectOptions {
            resume: true,
            ..ConnectOptions::default()
        },
    )
    .expect("reconnect");
    sw
}

// --- Early return after a one-way upstream cut (S2 ghost slot) --------------------------------

fn early_return(profile: Profile) {
    let mut w = World::new(profile, SessionSettings::default(), host_opts());
    let (mut c, old, peer) = joined(&mut w, SessionSettings::default(), ConnectOptions::default());
    play_until_flap(&mut w, &mut c);
    let slots = w.core().players_in_use();
    assert_eq!(slots, 1);
    w.server_events.clear();
    // The upstream dies; the downstream still works. The server sees silence, not a goodbye.
    old.mute.store(true, Ordering::Relaxed);
    w.run_for(&mut [&mut c], 2_000);
    assert_eq!(
        w.core().peer_state(peer),
        Some(PeerState::InGame),
        "{}: 2 s is well inside the server's timeout",
        profile.name
    );
    let _new = reconnect_with_token(&mut w, &mut c, "127.0.0.1:0");
    // Back in game before the old link's 20 s timeout would have freed anything.
    assert!(
        w.run_until(&mut [&mut c], 15_000, |_, cs| in_game(cs[0])),
        "{}: the reconnect reached in_game ({:?})",
        profile.name,
        c.state()
    );
    w.run_for(&mut [&mut c], 200);
    assert_eq!(
        welcomed_peer(&c.take_client_events()),
        Some(peer),
        "{}: the early return kept its PeerId",
        profile.name
    );
    assert!(resumed(&w.server_events, peer), "{}: PlayerResumed", profile.name);
    assert_eq!(
        w.core().peer_ids(),
        vec![peer],
        "{}: no second peer",
        profile.name
    );
    assert_eq!(
        w.core().players_in_use(),
        slots,
        "{}: slot count unchanged",
        profile.name
    );
    assert_eq!(
        left(&w.server_events, peer),
        None,
        "{}: the player never left",
        profile.name
    );
}

#[test]
fn early_return_resumes_the_same_peer_id_good_wifi() {
    early_return(GOOD_WIFI);
}

#[test]
fn early_return_resumes_the_same_peer_id_flappy() {
    early_return(FLAPPY);
}

// --- Slot reclaim on a full server (soak F6) ---------------------------------------------------

fn slot_reclaim(profile: Profile) {
    let opts = HostOptions {
        max_players: 2,
        ..host_opts()
    };
    let mut w = World::new(profile, SessionSettings::default(), opts);
    let (mut a, a_old, peer_a) = joined(&mut w, SessionSettings::default(), ConnectOptions::default());
    let (mut b, _, peer_b) = joined(&mut w, SessionSettings::default(), ConnectOptions::default());
    play_until_flap(&mut w, &mut a);
    assert_eq!(
        w.core().players_in_use(),
        2,
        "{}: the server is full",
        profile.name
    );
    w.server_events.clear();
    a_old.mute.store(true, Ordering::Relaxed);
    w.run_for(&mut [&mut a, &mut b], 3_000);
    let _ = reconnect_with_token(&mut w, &mut a, "127.0.0.1:0");
    assert!(
        w.run_until(&mut [&mut a, &mut b], 15_000, |_, cs| in_game(cs[0])),
        "{}: the returning player got back in ({:?})",
        profile.name,
        a.state()
    );
    let refused_full = w
        .server_events
        .iter()
        .any(|e| matches!(e, ServerSessionEvent::Refused { reason, .. } if reason.code() == "server_full"));
    assert!(
        !refused_full,
        "{}: its own ghost slot never refused it as full",
        profile.name
    );
    assert_eq!(
        welcomed_peer(&a.take_client_events()),
        Some(peer_a),
        "{}",
        profile.name
    );
    assert_eq!(w.core().peer_ids(), vec![peer_a, peer_b], "{}", profile.name);
    assert_eq!(w.core().players_in_use(), 2, "{}", profile.name);
}

#[test]
fn a_returning_player_reclaims_its_slot_on_a_full_server_good_wifi() {
    slot_reclaim(GOOD_WIFI);
}

#[test]
fn a_returning_player_reclaims_its_slot_on_a_full_server_flappy() {
    slot_reclaim(FLAPPY);
}

// --- Adversarial row 8: same-address rejoin of a quiet sealed peer ----------------------------

fn sealed() -> SessionSettings {
    SessionSettings {
        encryption: CryptoMode::Required,
        ..SessionSettings::default()
    }
}

fn same_address_rejoin(profile: Profile) {
    let mut w = World::new(profile, sealed(), host_opts());
    let (socket, old) = w.socket("127.0.0.1:41000");
    let mut c = NetSession::new(sealed(), manifest());
    c.set_manual_clock(w.clock.now());
    c.connect_on(socket, w.server_addr, ConnectOptions::default())
        .expect("connect");
    assert!(
        w.run_until(&mut [&mut c], 30_000, |_, cs| in_game(cs[0])),
        "{}",
        profile.name
    );
    w.run_for(&mut [&mut c], 200);
    let peer = welcomed_peer(&c.take_client_events()).expect("welcomed");
    play_until_flap(&mut w, &mut c);
    w.server_events.clear();
    // The client process dies quietly; 2 s later it restarts and the OS hands it the same port.
    old.mute.store(true, Ordering::Relaxed);
    old.deaf.store(true, Ordering::Relaxed);
    w.run_for(&mut [&mut c], 2_100);
    c.reset();
    w.run_for(&mut [], 16); // the old socket is gone with the old process
    let (socket, _) = w.socket("127.0.0.1:41000");
    c.set_manual_clock(w.clock.now());
    c.connect_on(
        socket,
        w.server_addr,
        ConnectOptions {
            resume: true,
            ..ConnectOptions::default()
        },
    )
    .expect("reconnect");
    assert!(
        w.run_until(&mut [&mut c], 15_000, |_, cs| in_game(cs[0])),
        "{}: the restarted client got back in ({:?})",
        profile.name,
        c.state()
    );
    w.run_for(&mut [&mut c], 200);
    assert_eq!(
        left(&w.server_events, peer),
        None,
        "{}: a same-address rejoin is a lost link, never a quit",
        profile.name
    );
    assert!(
        resumed(&w.server_events, peer),
        "{}: the old token resumed the slot",
        profile.name
    );
    assert_eq!(
        welcomed_peer(&c.take_client_events()),
        Some(peer),
        "{}",
        profile.name
    );
    assert_eq!(w.core().peer_ids(), vec![peer], "{}", profile.name);
}

#[test]
fn a_same_address_rejoin_enters_grace_and_resumes_good_wifi() {
    same_address_rejoin(GOOD_WIFI);
}

#[test]
fn a_same_address_rejoin_enters_grace_and_resumes_flappy() {
    same_address_rejoin(FLAPPY);
}

// --- Streams through a 5 s outage at 4x the default rate --------------------------------------

fn streams_survive_outage(profile: Profile, stream_share: f64, outage_ms: u64) {
    let rate = 4 * HostOptions::default().client_rate_bytes;
    let opts = HostOptions {
        client_rate_bytes: rate,
        ..host_opts()
    };
    let settings = SessionSettings {
        stream_share,
        ..SessionSettings::default()
    };
    let mut w = World::new(profile, settings.clone(), opts);
    let (mut c, sw, peer) = joined(
        &mut w,
        settings,
        ConnectOptions {
            client_rate_bytes: rate,
            ..ConnectOptions::default()
        },
    );
    // Large enough that every stream is still sending when the outage starts and after it ends.
    const STREAM_BYTES: usize = 1024 * 1024;
    let mut ids = Vec::new();
    for i in 0..3u8 {
        let bytes: Vec<u8> = (0..STREAM_BYTES).map(|n| (n as u8) ^ i).collect();
        ids.push(w.server.send_stream(peer, "blob", bytes).expect("stream"));
    }
    // Two seconds of sending, then the link goes dark both ways.
    w.run_for(&mut [&mut c], 2_000);
    sw.mute.store(true, Ordering::Relaxed);
    sw.deaf.store(true, Ordering::Relaxed);
    w.run_for(&mut [&mut c], outage_ms);
    sw.mute.store(false, Ordering::Relaxed);
    sw.deaf.store(false, Ordering::Relaxed);
    let mut complete = Vec::new();
    let mut sent = 0;
    let end = w.clock.now() + 180_000;
    while w.clock.now() < end && complete.len() < 3 {
        w.step(&mut [&mut c]);
        for e in c.take_client_events() {
            if let ClientSessionEvent::Stream(StreamEvent::Complete { kind, bytes, .. }) = e {
                assert_eq!(kind, "blob");
                assert_eq!(bytes.len(), STREAM_BYTES);
                complete.push(bytes);
            }
        }
        for e in std::mem::take(&mut w.server_events) {
            match e {
                ServerSessionEvent::Stream {
                    event: StreamEvent::Sent { .. },
                    ..
                } => sent += 1,
                ServerSessionEvent::Stream {
                    event: StreamEvent::Failed { id, reason },
                    ..
                } => panic!("{}: stream {id:?} failed: {reason}", profile.name),
                ServerSessionEvent::PlayerLeft { reason, .. }
                | ServerSessionEvent::PlayerDropped { reason, .. } => {
                    panic!("{}: the player was dropped: {}", profile.name, reason.code())
                }
                _ => {}
            }
        }
    }
    assert_eq!(
        complete.len(),
        3,
        "{}: every stream completed (sent {sent})",
        profile.name
    );
    for (i, bytes) in complete.iter().enumerate() {
        let tag = bytes[0];
        assert!(tag < 3, "{}: stream {i} payload", profile.name);
        assert!(
            bytes.iter().enumerate().all(|(n, b)| *b == (n as u8) ^ tag),
            "{}: stream {i} arrived intact",
            profile.name
        );
    }
    assert!(in_game(&c), "{}: still connected ({:?})", profile.name, c.state());
    assert_eq!(
        w.core().peer_state(peer),
        Some(PeerState::InGame),
        "{}",
        profile.name
    );
}

/// The brief's case: 5 s, 4x the default rate, the default 25 % stream share.
#[test]
fn three_streams_survive_a_five_second_outage_at_four_times_the_rate_good_wifi() {
    streams_survive_outage(GOOD_WIFI, SessionSettings::default().stream_share, 5_000);
}

#[test]
fn three_streams_survive_a_five_second_outage_at_four_times_the_rate_flappy() {
    streams_survive_outage(FLAPPY, SessionSettings::default().stream_share, 5_000);
}

/// DO27's 8 s stall edge (spec 120 § 10.1) with the whole budget given to streams: about 1.5 MB
/// queue in the outage, past the transport's 1024-fragment reliable window, which used to end
/// the player with `reliable_overflow` and no grace.
#[test]
fn three_streams_on_the_whole_budget_survive_an_eight_second_stall_good_wifi() {
    streams_survive_outage(GOOD_WIFI, 1.0, 8_000);
}

#[test]
fn three_streams_on_the_whole_budget_survive_an_eight_second_stall_flappy() {
    streams_survive_outage(FLAPPY, 1.0, 8_000);
}

/// The same stall upstream: the client's driver used to end the link on the first refusal too.
fn client_streams_survive_stall(profile: Profile) {
    let rate = 4 * HostOptions::default().client_rate_bytes;
    let settings = SessionSettings {
        stream_share: 1.0,
        ..SessionSettings::default()
    };
    let mut w = World::new(profile, settings.clone(), host_opts());
    let (mut c, sw, peer) = joined(
        &mut w,
        settings,
        ConnectOptions {
            client_rate_bytes: rate,
            ..ConnectOptions::default()
        },
    );
    const STREAM_BYTES: usize = 1024 * 1024;
    for i in 0..3u8 {
        let bytes: Vec<u8> = (0..STREAM_BYTES).map(|n| (n as u8) ^ i).collect();
        c.send_stream(PeerId::SERVER, "upload", bytes).expect("stream");
    }
    w.run_for(&mut [&mut c], 2_000);
    sw.mute.store(true, Ordering::Relaxed);
    sw.deaf.store(true, Ordering::Relaxed);
    w.run_for(&mut [&mut c], 8_000);
    sw.mute.store(false, Ordering::Relaxed);
    sw.deaf.store(false, Ordering::Relaxed);
    let mut complete = 0;
    let end = w.clock.now() + 180_000;
    while w.clock.now() < end && complete < 3 {
        w.step(&mut [&mut c]);
        for e in std::mem::take(&mut w.server_events) {
            match e {
                ServerSessionEvent::Stream {
                    event: StreamEvent::Complete { bytes, .. },
                    ..
                } => {
                    let tag = bytes[0];
                    assert!(
                        bytes.len() == STREAM_BYTES
                            && bytes.iter().enumerate().all(|(n, b)| *b == (n as u8) ^ tag),
                        "{}: upload intact",
                        profile.name
                    );
                    complete += 1;
                }
                ServerSessionEvent::PlayerLeft { reason, .. }
                | ServerSessionEvent::PlayerDropped { reason, .. } => {
                    panic!("{}: the player was dropped: {}", profile.name, reason.code())
                }
                _ => {}
            }
        }
        for e in c.take_client_events() {
            if let ClientSessionEvent::Stream(StreamEvent::Failed { id, reason }) = e {
                panic!("{}: upload {id:?} failed: {reason}", profile.name);
            }
        }
    }
    assert_eq!(complete, 3, "{}: every upload completed", profile.name);
    assert!(in_game(&c), "{}: still connected ({:?})", profile.name, c.state());
    assert_eq!(
        w.core().peer_state(peer),
        Some(PeerState::InGame),
        "{}",
        profile.name
    );
}

#[test]
fn client_uploads_on_the_whole_budget_survive_an_eight_second_stall_good_wifi() {
    client_streams_survive_stall(GOOD_WIFI);
}

#[test]
fn client_uploads_on_the_whole_budget_survive_an_eight_second_stall_flappy() {
    client_streams_survive_stall(FLAPPY);
}

// --- A ban after a NAT rebind bans the new address (S3) ---------------------------------------

fn ban_after_rebind(profile: Profile) {
    let mut w = World::new(profile, sealed(), host_opts());
    let (socket, _) = w.socket("127.0.0.1:0");
    let inner = Arc::clone(&socket.inner);
    let mut c = NetSession::new(sealed(), manifest());
    c.set_manual_clock(w.clock.now());
    c.connect_on(socket, w.server_addr, ConnectOptions::default())
        .expect("connect");
    assert!(
        w.run_until(&mut [&mut c], 30_000, |_, cs| in_game(cs[0])),
        "{}",
        profile.name
    );
    w.run_for(&mut [&mut c], 200);
    let peer = welcomed_peer(&c.take_client_events()).expect("welcomed");
    play_until_flap(&mut w, &mut c);
    // The NAT moves the client to a new public address.
    let moved = w.net.bind("127.0.0.9:0".parse().expect("addr")).expect("bind");
    let new_addr = moved.local_addr().expect("addr");
    *inner.lock().expect("socket") = moved;
    assert!(
        w.run_until(&mut [&mut c], 10_000, |w, _| w.core().peer_addr(peer)
            == Some(new_addr)),
        "{}: the session core follows the rebind (now {:?})",
        profile.name,
        w.core().peer_addr(peer)
    );
    let id =
        regia_net::server_host::ban_player(&mut w.server, &peer.0.to_string(), None, "test").expect("ban");
    let entry = w
        .core()
        .bans()
        .list()
        .entries
        .iter()
        .find(|e| e.id == id)
        .cloned()
        .expect("ban entry");
    assert_eq!(
        entry.value, "127.0.0.9",
        "{}: the ban hit the new IP",
        profile.name
    );
}

#[test]
fn a_ban_after_a_rebind_bans_the_new_ip_good_wifi() {
    ban_after_rebind(GOOD_WIFI);
}

#[test]
fn a_ban_after_a_rebind_bans_the_new_ip_flappy() {
    ban_after_rebind(FLAPPY);
}

// --- Adversarial row 7: live-token takeover from every live state -----------------------------

fn hello(token: Option<[u8; 16]>) -> Vec<u8> {
    Hello {
        protocol_version: 1,
        protocol_hash: 0xabc,
        manifest: manifest(),
        player_names: vec!["ada".into()],
        password: None,
        auth: None,
        client_rate_bytes: 0,
        reconnect_token: token,
    }
    .encode()
    .expect("hello")
}

fn core() -> SessionServer {
    let mut s = SessionServer::new(SessionSettings::default(), host_opts(), manifest(), [7; 32]);
    s.set_dev_build(false);
    s
}

fn welcome_token(s: &mut SessionServer, link: LinkId) -> [u8; 16] {
    s.take_outgoing()
        .into_iter()
        .filter(|o| o.link == link)
        .find_map(|o| match SessionMsg::decode(&o.bytes) {
            Ok(SessionMsg::Welcome(w)) => Some(w.reconnect_token),
            _ => None,
        })
        .expect("a welcome on the link")
}

#[derive(Debug, Clone, Copy)]
enum Live {
    Loading,
    Joining,
    InGame,
    Interrupted,
}

fn takeover_from(state: Live) {
    let mut s = core();
    let a: SocketAddr = "10.0.0.1:5000".parse().expect("addr");
    let b: SocketAddr = "10.0.0.2:6000".parse().expect("addr");
    let mut now = 0;
    s.link_up(LinkId(1), a, now);
    s.receive(LinkId(1), &hello(None), now);
    let token = welcome_token(&mut s, LinkId(1));
    let peer = s.peer_of(LinkId(1)).expect("peer");
    let travel = s.travel_id();
    if !matches!(state, Live::Loading) {
        let ready = SessionMsg::LevelReady { travel_id: travel }
            .encode()
            .expect("msg");
        s.receive(LinkId(1), &ready, now);
    }
    if matches!(state, Live::InGame | Live::Interrupted) {
        let ig = SessionMsg::InGame { tick: 0 }.encode().expect("msg");
        s.receive(LinkId(1), &ig, now);
    }
    if matches!(state, Live::Interrupted) {
        now += 2_000;
        s.update(now);
        assert!(s.peer_rows()[0].interrupted, "{state:?}");
    }
    let before = s.players_in_use();
    while s.poll_event().is_some() {}
    let _ = s.take_outgoing();
    // The same player connects from a new address with its current token, the goodbye never
    // came and the server's timeout has not fired.
    now += 100;
    s.link_up(LinkId(2), b, now);
    s.receive(LinkId(2), &hello(Some(token)), now);
    let events: Vec<ServerSessionEvent> = std::iter::from_fn(|| s.poll_event()).collect();
    assert!(resumed(&events, peer), "{state:?}: PlayerResumed, got {events:?}");
    assert_eq!(s.peer_ids(), vec![peer], "{state:?}: no second peer");
    assert_eq!(s.players_in_use(), before, "{state:?}: no new allocation");
    assert_eq!(
        s.link_of(peer),
        Some(LinkId(2)),
        "{state:?}: the new link carries the peer"
    );
    assert_eq!(s.peer_of(LinkId(1)), None, "{state:?}: the old link is detached");
    assert_eq!(s.peer_addr(peer), Some(b), "{state:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ServerSessionEvent::PlayerLeft { .. })),
        "{state:?}: nobody left"
    );
    // The new welcome rotates the token: the one just used is spent.
    let fresh = welcome_token(&mut s, LinkId(2));
    assert_ne!(fresh, token, "{state:?}");
}

#[test]
fn a_live_token_takes_over_a_loading_peer() {
    takeover_from(Live::Loading);
}

#[test]
fn a_live_token_takes_over_a_joining_peer() {
    takeover_from(Live::Joining);
}

#[test]
fn a_live_token_takes_over_an_in_game_peer() {
    takeover_from(Live::InGame);
}

#[test]
fn a_live_token_takes_over_an_interrupted_peer() {
    takeover_from(Live::Interrupted);
}

// --- Adversarial row 9: an authentic Join resend refreshes liveness ---------------------------

#[test]
fn join_resends_keep_an_admitted_peer_alive() {
    let net = LoopbackNet::new();
    let clock = Clock::default();
    let saddr: SocketAddr = "127.0.0.1:7900".parse().expect("addr");
    let timeouts = TimeoutPolicy {
        start_grace_ms: 0,
        ..TimeoutPolicy::default()
    };
    let timeout_ms = timeouts.timeout_ms;
    let mut server: NetServer<SessionWire, _> = NetServer::with_secret(
        net.bind(saddr).expect("bind"),
        ServerConfig {
            crypto: CryptoMode::Required,
            health: HealthPolicy::default().with_timeouts(timeouts),
            ..ServerConfig::default()
        },
        [3; 32],
    );
    let (socket, sw) = Impaired::new(
        net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
        &clock,
        GOOD_WIFI,
        9,
    );
    let mut client: NetClient<SessionWire, _> = NetClient::connect(
        socket,
        saddr,
        ClientConfig {
            crypto: CryptoMode::Required,
            connect_timeout_ms: 10 * timeout_ms,
            ..ClientConfig::default()
        },
        &(),
    )
    .expect("connect");
    // Handshake until the server admits the client, then drop every server datagram: the accept
    // is lost and the client keeps resending its sealed Join.
    let mut admitted_at = None;
    for _ in 0..2_000 {
        let now = clock.advance(STEP_MS);
        client.update(now);
        server.update(now);
        if server.session().len() == 1 {
            sw.deaf.store(true, Ordering::Relaxed);
            admitted_at = Some(now);
            break;
        }
    }
    let admitted_at = admitted_at.expect("the server admitted the client");
    while clock.now() < admitted_at + timeout_ms + 1 {
        let now = clock.advance(STEP_MS);
        client.update(now);
        server.update(now);
        assert!(
            matches!(client.state(), ClientState::Connecting),
            "the client stays inside its connect window: {:?}",
            client.state()
        );
    }
    assert_eq!(
        server.session().len(),
        1,
        "authentic Join resends for timeout_ms + 1 keep the peer present"
    );
    // Only a gap with no authentic datagram removes it.
    sw.mute.store(true, Ordering::Relaxed);
    let quiet_from = clock.now();
    while clock.now() < quiet_from + timeout_ms + 100 {
        let now = clock.advance(STEP_MS);
        client.update(now);
        server.update(now);
    }
    assert_eq!(server.session().len(), 0, "silence for timeout_ms removes it");
}

// --- Automatic reconnect after a timeout (spec 120 § 10.3, ADR-8352) --------------------------

/// long-outage, 25 s past the 20 s timeout, on a session client of a caller-owned socket: the
/// session reconnects by itself on that socket with its token and is the same `PeerId` again;
/// the game sees one `PeerInterrupted`, numbered `Reconnecting` attempts, one `PeerRestored`, and
/// no `Disconnected`; the server never lets the player go.
#[test]
fn a_timed_out_session_client_reconnects_by_itself_with_the_same_peer_id() {
    let mut w = World::new(GOOD_WIFI, SessionSettings::default(), host_opts());
    let (mut c, sw, peer) = joined(&mut w, SessionSettings::default(), ConnectOptions::default());
    w.run_for(&mut [&mut c], 2_000);
    let _ = c.take_client_events();
    w.server_events.clear();
    sw.mute.store(true, Ordering::Relaxed);
    sw.deaf.store(true, Ordering::Relaxed);
    let mut events = Vec::new();
    let end = w.clock.now() + 45_000;
    while w.clock.now() < end {
        w.step(&mut [&mut c]);
        events.extend(c.take_client_events());
    }
    assert!(c.is_reconnecting(), "reconnecting by itself: {events:?}");
    let status = c.reconnect_status().expect("status");
    assert!(status.attempt >= 4 && status.window_left_ms > 0, "{status:?}");
    sw.mute.store(false, Ordering::Relaxed);
    sw.deaf.store(false, Ordering::Relaxed);
    let back = {
        let end = w.clock.now() + 10_000;
        let mut at = None;
        while w.clock.now() < end {
            w.step(&mut [&mut c]);
            events.extend(c.take_client_events());
            if in_game(&c) {
                at = Some(w.clock.now());
                break;
            }
        }
        at
    };
    assert!(back.is_some(), "back in game: {:?} {events:?}", c.state());
    w.run_for(&mut [&mut c], 500);
    events.extend(c.take_client_events());
    assert_eq!(welcomed_peer(&events), Some(peer), "the same PeerId");
    assert!(!c.is_reconnecting());
    assert!(resumed(&w.server_events, peer), "PlayerResumed");
    assert_eq!(left(&w.server_events, peer), None, "the player never left");
    assert_eq!(w.core().peer_ids(), vec![peer]);
    let count = |f: &dyn Fn(&ClientSessionEvent) -> bool| events.iter().filter(|e| f(e)).count();
    assert_eq!(
        count(&|e| matches!(e, ClientSessionEvent::PeerInterrupted)),
        1,
        "{events:?}"
    );
    assert_eq!(
        count(&|e| matches!(e, ClientSessionEvent::PeerRestored)),
        1,
        "{events:?}"
    );
    assert_eq!(
        count(&|e| matches!(
            e,
            ClientSessionEvent::Disconnected { .. } | ClientSessionEvent::ReconnectFailed { .. }
        )),
        0,
        "{events:?}"
    );
    let attempts: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            ClientSessionEvent::Reconnecting { attempt } => Some(*attempt),
            _ => None,
        })
        .collect();
    assert_eq!(
        attempts,
        (1..=attempts.len() as u32).collect::<Vec<_>>(),
        "{events:?}"
    );
    assert!(attempts.len() >= 4);
}

/// The window ends: one `ReconnectFailed`, then one `Disconnected { timeout }`, then nothing;
/// `ReconnectPolicy::off()` ends at the timeout as before.
#[test]
fn a_session_reconnect_gives_up_once_and_off_keeps_the_old_end() {
    for off in [false, true] {
        let mut w = World::new(GOOD_WIFI, SessionSettings::default(), host_opts());
        let (mut c, sw, _peer) = joined(&mut w, SessionSettings::default(), ConnectOptions::default());
        if off {
            c.set_reconnect_policy(regia_net::reconnect::ReconnectPolicy::off());
        }
        let _ = c.take_client_events();
        sw.mute.store(true, Ordering::Relaxed);
        sw.deaf.store(true, Ordering::Relaxed);
        let mut events = Vec::new();
        let end = w.clock.now() + 110_000;
        while w.clock.now() < end {
            w.step(&mut [&mut c]);
            events.extend(c.take_client_events());
        }
        let failed = events
            .iter()
            .filter(|e| matches!(e, ClientSessionEvent::ReconnectFailed { .. }))
            .count();
        let disconnected: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ClientSessionEvent::Disconnected { reason } => Some(reason.code().to_owned()),
                _ => None,
            })
            .collect();
        let attempts = events
            .iter()
            .filter(|e| matches!(e, ClientSessionEvent::Reconnecting { .. }))
            .count();
        assert_eq!(disconnected, vec!["timeout".to_owned()], "off {off}: {events:?}");
        if off {
            assert_eq!((failed, attempts), (0, 0), "{events:?}");
        } else {
            assert_eq!(failed, 1, "{events:?}");
            assert!((8..=11).contains(&attempts), "{events:?}");
        }
        assert!(!c.is_reconnecting());
    }
}

// --- ADR-8535: a lost welcome must not turn a resume into a fresh join ---------------------------

/// A peer that joined on link 1 and went in game; returns its peer and token.
fn in_game_core(s: &mut SessionServer, now: u64) -> (PeerId, [u8; 16]) {
    let a: SocketAddr = "10.0.0.1:5000".parse().expect("addr");
    s.link_up(LinkId(1), a, now);
    s.receive(LinkId(1), &hello(None), now);
    let token = welcome_token(s, LinkId(1));
    let peer = s.peer_of(LinkId(1)).expect("peer");
    let ready = SessionMsg::LevelReady {
        travel_id: s.travel_id(),
    }
    .encode()
    .expect("msg");
    s.receive(LinkId(1), &ready, now);
    let ig = SessionMsg::InGame { tick: 0 }.encode().expect("msg");
    s.receive(LinkId(1), &ig, now);
    while s.poll_event().is_some() {}
    let _ = s.take_outgoing();
    (peer, token)
}

fn token_refused(events: &[ServerSessionEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, ServerSessionEvent::TokenRefused { .. }))
        .count()
}

/// The resume on link 2 is taken, but its welcome (the new token) is lost with the link; the
/// client's next attempt on link 3 presents the same token. It is the same peer: no
/// `TokenRefused`, no second peer.
#[test]
fn a_lost_welcome_does_not_turn_the_retry_into_a_fresh_join() {
    let mut s = core();
    let (peer, token) = in_game_core(&mut s, 0);
    let b: SocketAddr = "10.0.0.2:6000".parse().expect("addr");
    let c: SocketAddr = "10.0.0.3:7000".parse().expect("addr");
    s.link_up(LinkId(2), b, 100);
    s.receive(LinkId(2), &hello(Some(token)), 100);
    let first: Vec<ServerSessionEvent> = std::iter::from_fn(|| s.poll_event()).collect();
    assert!(resumed(&first, peer), "{first:?}");
    // The welcome never arrives.
    let _ = s.take_outgoing();
    for retry in 0..2u32 {
        let link = LinkId(3 + retry);
        s.link_up(link, c, 200 + u64::from(retry));
        s.receive(link, &hello(Some(token)), 200 + u64::from(retry));
        let events: Vec<ServerSessionEvent> = std::iter::from_fn(|| s.poll_event()).collect();
        assert!(resumed(&events, peer), "retry {retry}: {events:?}");
        assert_eq!(token_refused(&events), 0, "retry {retry}: {events:?}");
        assert_eq!(s.peer_ids(), vec![peer], "retry {retry}: no second peer");
        assert_eq!(s.link_of(peer), Some(link), "retry {retry}");
        if retry == 0 {
            // Lost again.
            let _ = s.take_outgoing();
        }
    }
    let fresh = welcome_token(&mut s, LinkId(4));
    assert_ne!(fresh, token, "the heard welcome still rotates the token");
}

/// Adversarial: once the resumed client shows it holds the welcome (its `LevelReady`), the token
/// it resumed with is spent. A replay on another link is `TokenRefused` and a fresh join; the
/// owner keeps its link and its peer.
#[test]
fn a_session_token_is_spent_once_the_resumed_client_shows_its_welcome() {
    let mut s = core();
    let (peer, token) = in_game_core(&mut s, 0);
    let b: SocketAddr = "10.0.0.2:6000".parse().expect("addr");
    let thief: SocketAddr = "10.9.9.9:9000".parse().expect("addr");
    s.link_up(LinkId(2), b, 100);
    s.receive(LinkId(2), &hello(Some(token)), 100);
    let newest = welcome_token(&mut s, LinkId(2));
    let ready = SessionMsg::LevelReady {
        travel_id: s.travel_id(),
    }
    .encode()
    .expect("msg");
    s.receive(LinkId(2), &ready, 150);
    while s.poll_event().is_some() {}
    s.link_up(LinkId(3), thief, 200);
    s.receive(LinkId(3), &hello(Some(token)), 200);
    let events: Vec<ServerSessionEvent> = std::iter::from_fn(|| s.poll_event()).collect();
    assert!(!resumed(&events, peer), "{events:?}");
    assert_eq!(token_refused(&events), 1, "{events:?}");
    assert_eq!(s.link_of(peer), Some(LinkId(2)), "the owner keeps its link");
    assert_ne!(s.peer_of(LinkId(3)), Some(peer), "the replay is not the owner");
    // A keepalive or a hello copy on the resumed link proves nothing: before the client's
    // `LevelReady` the old token still resumes (tested above); the newest always does.
    s.link_up(LinkId(4), b, 300);
    s.receive(LinkId(4), &hello(Some(newest)), 300);
    let events: Vec<ServerSessionEvent> = std::iter::from_fn(|| s.poll_event()).collect();
    assert!(resumed(&events, peer), "the newest token resumes: {events:?}");
}

/// A keepalive on the resumed link does not spend the presented token: a client that has not
/// heard its welcome sends keepalives too.
#[test]
fn a_keepalive_does_not_spend_the_retry_token() {
    let mut s = core();
    let (peer, token) = in_game_core(&mut s, 0);
    let b: SocketAddr = "10.0.0.2:6000".parse().expect("addr");
    let c: SocketAddr = "10.0.0.3:7000".parse().expect("addr");
    s.link_up(LinkId(2), b, 100);
    s.receive(LinkId(2), &hello(Some(token)), 100);
    let _ = s.take_outgoing();
    let keepalive = SessionMsg::Keepalive.encode().expect("msg");
    s.receive(LinkId(2), &keepalive, 150);
    while s.poll_event().is_some() {}
    s.link_up(LinkId(3), c, 200);
    s.receive(LinkId(3), &hello(Some(token)), 200);
    let events: Vec<ServerSessionEvent> = std::iter::from_fn(|| s.poll_event()).collect();
    assert!(resumed(&events, peer), "{events:?}");
    assert_eq!(s.peer_ids(), vec![peer]);
}

/// A resume puts the peer back in `Loading`. The link then dies (transport timeout) before
/// `LevelReady`. The slot must stay in grace: the token the client still holds, and the welcome
/// token it already heard, both resume that same peer. Retiring the peer here makes the legitimate
/// retry a fresh join.
#[test]
fn a_drop_while_loading_after_a_resume_keeps_the_slot() {
    let mut s = core();
    let (peer, token) = in_game_core(&mut s, 0);
    s.link_down(LinkId(1), DisconnectReason::Timeout, 1_000);
    assert_eq!(
        s.peer_state(peer),
        Some(PeerState::Grace),
        "in-game timeout enters grace"
    );
    let b: SocketAddr = "10.0.0.2:6000".parse().expect("addr");
    s.link_up(LinkId(2), b, 2_000);
    s.receive(LinkId(2), &hello(Some(token)), 2_000);
    let first: Vec<ServerSessionEvent> = std::iter::from_fn(|| s.poll_event()).collect();
    assert!(resumed(&first, peer), "{first:?}");
    assert_eq!(s.peer_state(peer), Some(PeerState::Loading));
    // The welcome never arrives. The resumed link then times out while the peer is still loading.
    let _ = s.take_outgoing();
    s.link_down(LinkId(2), DisconnectReason::Timeout, 3_000);
    let c: SocketAddr = "10.0.0.3:7000".parse().expect("addr");
    s.link_up(LinkId(3), c, 4_000);
    s.receive(LinkId(3), &hello(Some(token)), 4_000);
    let events: Vec<ServerSessionEvent> = std::iter::from_fn(|| s.poll_event()).collect();
    assert!(
        resumed(&events, peer),
        "the token the client still holds must resume the loading slot: {events:?}"
    );
    assert_eq!(
        s.peer_ids(),
        vec![peer],
        "Loading + Timeout must keep the slot, not retire it (state {:?})",
        s.peer_state(peer)
    );
}

/// The client heard the welcome (it holds the new token) and the link died before `LevelReady`.
/// That token must still name the same peer.
#[test]
fn a_welcome_token_survives_a_drop_while_loading() {
    let mut s = core();
    let (peer, token) = in_game_core(&mut s, 0);
    s.link_down(LinkId(1), DisconnectReason::Timeout, 1_000);
    let b: SocketAddr = "10.0.0.2:6000".parse().expect("addr");
    s.link_up(LinkId(2), b, 2_000);
    s.receive(LinkId(2), &hello(Some(token)), 2_000);
    let newest = welcome_token(&mut s, LinkId(2));
    assert_ne!(newest, token, "the welcome rotates the token");
    while s.poll_event().is_some() {}
    s.link_down(LinkId(2), DisconnectReason::Timeout, 3_000);
    let c: SocketAddr = "10.0.0.3:7000".parse().expect("addr");
    s.link_up(LinkId(3), c, 4_000);
    s.receive(LinkId(3), &hello(Some(newest)), 4_000);
    let events: Vec<ServerSessionEvent> = std::iter::from_fn(|| s.poll_event()).collect();
    assert!(
        resumed(&events, peer),
        "the welcome the client already holds must resume the loading slot: {events:?}"
    );
    assert_eq!(s.peer_ids(), vec![peer], "{events:?}");
}

/// The retry token is one deep. The client heard welcome T1 and has not sent `LevelReady`, so T0
/// is not confirmed away. A later attempt presents T1 (the client refreshed). A hello that still
/// carries T0 — the previous attempt, still in flight — must be the same peer.
#[test]
fn an_older_session_token_survives_until_level_ready() {
    let mut s = core();
    let (peer, t0) = in_game_core(&mut s, 0);
    let b: SocketAddr = "10.0.0.2:6000".parse().expect("addr");
    let c: SocketAddr = "10.0.0.3:7000".parse().expect("addr");
    let d: SocketAddr = "10.0.0.4:8000".parse().expect("addr");
    s.link_up(LinkId(2), b, 100);
    s.receive(LinkId(2), &hello(Some(t0)), 100);
    let t1 = welcome_token(&mut s, LinkId(2));
    assert_ne!(t1, t0);
    while s.poll_event().is_some() {}
    s.link_up(LinkId(3), c, 200);
    s.receive(LinkId(3), &hello(Some(t1)), 200);
    let mid: Vec<ServerSessionEvent> = std::iter::from_fn(|| s.poll_event()).collect();
    assert!(resumed(&mid, peer), "T1 is the unconfirmed welcome: {mid:?}");
    assert_eq!(s.peer_state(peer), Some(PeerState::Loading));
    s.link_up(LinkId(4), d, 300);
    s.receive(LinkId(4), &hello(Some(t0)), 300);
    let events: Vec<ServerSessionEvent> = std::iter::from_fn(|| s.poll_event()).collect();
    assert!(
        resumed(&events, peer),
        "T0 was not confirmed away by LevelReady: {events:?}"
    );
    assert_eq!(s.peer_ids(), vec![peer], "{events:?}");
    assert_eq!(token_refused(&events), 0, "{events:?}");
}

/// ADR-8623 lane tests: the unconfirmed token chain is bounded, lapses, and is spent whole by the
/// client's confirmation; only a resume's load keeps the grace slot on a lost link.
fn session_msg(msg: SessionMsg) -> Vec<u8> {
    msg.encode().expect("msg")
}

/// A resume on `link` from a fresh address that presents `token`; returns the welcome's token.
fn resume_on(s: &mut SessionServer, link: u32, token: [u8; 16], now: u64) -> [u8; 16] {
    let addr: SocketAddr = format!("10.0.1.{}:{}", link % 250, 6000 + link)
        .parse()
        .expect("addr");
    s.link_up(LinkId(link), addr, now);
    s.receive(LinkId(link), &hello(Some(token)), now);
    welcome_token(s, LinkId(link))
}

/// Three resumes in a row, none confirmed: every token of the chain still names the peer. Then
/// the client's `LevelReady` on the newest link spends them all: each is `TokenRefused` and a
/// fresh join, and the owner keeps its slot.
#[test]
fn level_ready_spends_the_whole_unconfirmed_token_chain() {
    let mut s = core();
    let (peer, t0) = in_game_core(&mut s, 0);
    let t1 = resume_on(&mut s, 2, t0, 100);
    let t2 = resume_on(&mut s, 3, t1, 200);
    let t3 = resume_on(&mut s, 4, t2, 300);
    while s.poll_event().is_some() {}
    for (i, old) in [t0, t1, t2].into_iter().enumerate() {
        let link = 10 + i as u32;
        let newest = resume_on(&mut s, link, old, 400 + i as u64);
        let events: Vec<ServerSessionEvent> = std::iter::from_fn(|| s.poll_event()).collect();
        assert!(resumed(&events, peer), "token {i} of the chain: {events:?}");
        assert_eq!(token_refused(&events), 0, "{events:?}");
        assert_ne!(newest, t3);
    }
    // The client confirms on the newest link (link 12).
    let ready = session_msg(SessionMsg::LevelReady {
        travel_id: s.travel_id(),
    });
    s.receive(LinkId(12), &ready, 500);
    while s.poll_event().is_some() {}
    let _ = s.take_outgoing();
    for (i, old) in [t0, t1, t2, t3].into_iter().enumerate() {
        let link = 20 + i as u32;
        let addr: SocketAddr = format!("10.0.2.{i}:7000").parse().expect("addr");
        s.link_up(LinkId(link), addr, 600);
        s.receive(LinkId(link), &hello(Some(old)), 600);
        let events: Vec<ServerSessionEvent> = std::iter::from_fn(|| s.poll_event()).collect();
        assert_eq!(token_refused(&events), 1, "token {i} is spent: {events:?}");
        assert!(!resumed(&events, peer), "{events:?}");
    }
    assert_eq!(
        s.peer_state(peer),
        Some(PeerState::Joining),
        "the owner keeps its slot"
    );
    assert_eq!(s.peer_of(LinkId(12)), Some(peer));
}

/// Ten resumes in a row, each presenting the newest token, none confirmed: the chain keeps at most
/// eight older tokens. The two superseded longest ago are gone (a fresh join); the rest still
/// resume the peer.
#[test]
fn the_unconfirmed_token_chain_is_bounded() {
    let mut s = core();
    let (peer, t0) = in_game_core(&mut s, 0);
    let mut chain = vec![t0];
    for k in 0..10u32 {
        let newest = *chain.last().expect("a token");
        chain.push(resume_on(&mut s, 2 + k, newest, 100 * u64::from(k + 1)));
    }
    while s.poll_event().is_some() {}
    // chain[0..10] are superseded; chain[10] is the newest.
    for (i, old) in chain[..10].iter().enumerate() {
        let link = 50 + i as u32;
        let addr: SocketAddr = format!("10.0.3.{i}:7000").parse().expect("addr");
        s.link_up(LinkId(link), addr, 2_000);
        s.receive(LinkId(link), &hello(Some(*old)), 2_000);
        let events: Vec<ServerSessionEvent> = std::iter::from_fn(|| s.poll_event()).collect();
        if i < 2 {
            assert_eq!(token_refused(&events), 1, "token {i} was evicted: {events:?}");
            // Leave the fresh join out of the rest of the test.
            s.link_down(LinkId(link), DisconnectReason::ClientQuit, 2_000);
            while s.poll_event().is_some() {}
        } else {
            assert!(resumed(&events, peer), "token {i} is kept: {events:?}");
            break;
        }
    }
}

/// A superseded token nobody presents lapses one lifetime after it was replaced:
/// `max(reconnect_grace_s, loading_timeout_s)`, 120 s by default.
#[test]
fn an_unconfirmed_token_lapses_after_its_lifetime() {
    let lifetime = {
        let d = SessionSettings::default();
        (d.reconnect_grace_s.max(d.loading_timeout_s) * 1_000.0) as u64
    };
    for (at, kept) in [(100 + lifetime - 1, true), (100 + lifetime, false)] {
        let mut s = core();
        let (peer, t0) = in_game_core(&mut s, 0);
        let _t1 = resume_on(&mut s, 2, t0, 100);
        while s.poll_event().is_some() {}
        s.link_up(LinkId(3), "10.0.0.3:7000".parse().expect("addr"), at);
        s.receive(LinkId(3), &hello(Some(t0)), at);
        let events: Vec<ServerSessionEvent> = std::iter::from_fn(|| s.poll_event()).collect();
        assert_eq!(resumed(&events, peer), kept, "at {at}: {events:?}");
        assert_eq!(token_refused(&events), usize::from(!kept), "at {at}: {events:?}");
    }
}

/// Only a resume's load keeps the grace slot: a fresh joiner that drops during its first load
/// still leaves, and so does a player that resumed, got back in game, and then drops during a
/// travel's load (both unchanged by ADR-8623).
#[test]
fn a_drop_while_loading_without_a_resume_still_leaves() {
    let mut s = core();
    let a: SocketAddr = "10.0.0.1:5000".parse().expect("addr");
    s.link_up(LinkId(1), a, 0);
    s.receive(LinkId(1), &hello(None), 0);
    let fresh = s.peer_of(LinkId(1)).expect("peer");
    assert_eq!(s.peer_state(fresh), Some(PeerState::Loading));
    s.link_down(LinkId(1), DisconnectReason::Timeout, 100);
    assert_eq!(s.peer_state(fresh), None, "a fresh joiner's load drop retires it");

    let mut s = core();
    let (peer, t0) = in_game_core(&mut s, 0);
    let _ = resume_on(&mut s, 2, t0, 100);
    s.receive(
        LinkId(2),
        &session_msg(SessionMsg::LevelReady {
            travel_id: s.travel_id(),
        }),
        200,
    );
    s.receive(LinkId(2), &session_msg(SessionMsg::InGame { tick: 0 }), 300);
    assert_eq!(s.peer_state(peer), Some(PeerState::InGame));
    s.travel("next_level");
    assert_eq!(s.peer_state(peer), Some(PeerState::Loading));
    s.link_down(LinkId(2), DisconnectReason::Timeout, 400);
    assert_eq!(s.peer_state(peer), None, "a travel's load drop retires as before");
}

/// The grace a resume's drop enters is a real grace: it ends after `reconnect_grace_s` like an
/// in-game drop's, and the slot is gone.
#[test]
fn the_grace_after_a_drop_while_loading_still_ends() {
    let mut s = core();
    let (peer, t0) = in_game_core(&mut s, 0);
    let _ = resume_on(&mut s, 2, t0, 100);
    s.link_down(LinkId(2), DisconnectReason::Timeout, 200);
    assert_eq!(s.peer_state(peer), Some(PeerState::Grace));
    let grace = (SessionSettings::default().reconnect_grace_s * 1_000.0) as u64;
    s.update(200 + grace - 1);
    assert_eq!(s.peer_state(peer), Some(PeerState::Grace));
    s.update(200 + grace);
    assert_eq!(s.peer_state(peer), None, "the grace ended");
}
