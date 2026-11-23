// SPDX-License-Identifier: MPL-2.0
//! Automatic client reconnect over `NetLink` (spec 120 § 10.2 and § 10.3, ADR-8352, plan N217).
//!
//! The client half of resume: N216 (`link_resume.rs`) keeps a timed-out player dormant and lets the
//! game call `NetLink::resume`; here the client does it by itself. Profiles of spec 120 § 15.1 on
//! the in-memory network with manual clocks (no wall clock, no sleeps):
//!
//! - long-outage: two-way 100 % loss for 45 s, 25 s past the 20 s timeout. The client retries with
//!   backoff and resumes the same `PeerId` within 2 s of the link returning; the game sees one
//!   `PeerInterrupted`, numbered `Reconnecting` attempts, one `PeerRestored`, and no `Ended`.
//! - the window ends: one `ReconnectFailed`, then one `Ended`, then silence.
//! - wifi-to-cell: address change + 2 s outage + RTT step 15 -> 60 ms one-way + loss 0.5 -> 3 %,
//!   20 seeded runs, 0 lost slots.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use regia_app::App;
use regia_net::bridge::NetFrame;
use regia_net::link::{LinkClock, LinkEvent, LinkOptions, LinkSocketOpener, NetLink, NetLinkPlugin};
use regia_net::reconnect::ReconnectPolicy;
use regia_net::resume::NetResumeState;
use regia_net::runtime::NetNode;
use regia_net::socket::{DatagramSocket, LoopbackNet, LoopbackSocket};
use regia_net::status::NetAgentState;
use regia_net::{NetGame, NetMode, NetPlugin, PeerId};

#[derive(Debug)]
struct ReconnectGame;

impl NetGame for ReconnectGame {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.link_reconnect";
    const PROTOCOL_VERSION: u32 = 1;
}

/// One app update of 60 Hz.
const STEP_US: u64 = 16_667;
const FRAMES_PER_S: u64 = 60;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn options() -> LinkOptions {
    LinkOptions {
        clock: LinkClock::Manual {
            start_ms: 0,
            step_us: STEP_US,
        },
        ..LinkOptions::default()
    }
}

fn app(mode: NetMode) -> App {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
    app
}

/// A socket whose path can be cut both ways (the long-outage profile's 100 % two-way loss).
struct GateSocket {
    socket: LoopbackSocket,
    down: Arc<AtomicBool>,
}

impl DatagramSocket for GateSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if self.down.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.socket.send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        loop {
            let got = self.socket.recv_from(buf)?;
            if got.is_none() || !self.down.load(Ordering::Relaxed) {
                return Ok(got);
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

struct Rig {
    server: App,
    a: App,
    /// The game's view of A's link events, in order.
    a_events: Vec<LinkEvent>,
    server_events: Vec<LinkEvent>,
    frame: u64,
}

impl Rig {
    fn new(server: App, a: App) -> Self {
        let mut rig = Self {
            server,
            a,
            a_events: Vec::new(),
            server_events: Vec::new(),
            frame: 0,
        };
        rig.run(3 * FRAMES_PER_S);
        assert!(rig.a.world().resource::<NetLink>().is_connected(), "a connected");
        rig
    }

    fn step(&mut self) {
        self.server.update();
        let events = self.server.world_mut().resource_mut::<NetLink>().take_events();
        self.server_events.extend(events);
        self.a.update();
        let events = self.a.world_mut().resource_mut::<NetLink>().take_events();
        self.a_events.extend(events);
        self.frame += 1;
    }

    fn run(&mut self, frames: u64) {
        for _ in 0..frames {
            self.step();
        }
    }

    /// Steps until A is connected as `peer` again; returns the frames it took.
    fn until_back(&mut self, peer: PeerId, limit: u64) -> Option<u64> {
        for n in 0..limit {
            if self.a_peer() == Some(peer) && self.a.world().resource::<NetLink>().is_connected() {
                return Some(n);
            }
            self.step();
        }
        None
    }

    fn a_peer(&self) -> Option<PeerId> {
        let link = self.a.world().resource::<NetLink>();
        link.status()
            .filter(|s| s.state == "connected")
            .and_then(|s| s.peer_id)
            .map(PeerId)
    }

    fn count(&self, f: impl Fn(&LinkEvent) -> bool) -> usize {
        self.a_events.iter().filter(|e| f(e)).count()
    }
}

/// A server and one client A on a gated socket (`opener`: reconnect on fresh sockets).
fn gated_rig(port: u16, opener: bool) -> (Rig, Arc<AtomicBool>, Arc<AtomicU32>) {
    let net = LoopbackNet::new();
    let mut server = app(NetMode::DedicatedServer);
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<ReconnectGame>(Box::new(net.bind(addr(port)).expect("bind")), &options())
        .expect("host");
    let down = Arc::new(AtomicBool::new(false));
    let opened = Arc::new(AtomicU32::new(0));
    let mut a = app(NetMode::Client);
    {
        let mut link = a.world_mut().resource_mut::<NetLink>();
        if opener {
            let (net, down, opened) = (net.clone(), down.clone(), opened.clone());
            let open: LinkSocketOpener = Arc::new(move || {
                opened.fetch_add(1, Ordering::Relaxed);
                Ok(Box::new(GateSocket {
                    socket: net.bind(addr(0))?,
                    down: down.clone(),
                }))
            });
            link.connect_via::<ReconnectGame>(open, addr(port), &options())
                .expect("connect");
        } else {
            link.connect::<ReconnectGame>(
                Box::new(GateSocket {
                    socket: net.bind(addr(0)).expect("bind"),
                    down: down.clone(),
                }),
                addr(port),
                &options(),
            )
            .expect("connect");
        }
    }
    (Rig::new(server, a), down, opened)
}

/// Long-outage, 25 s past the 20 s timeout (45 s of two-way loss): the client reconnects by
/// itself and is the same `PeerId` within 2 s of the link returning; one Interrupted and one
/// Restored for the outage, numbered attempts, no `Ended`.
fn long_outage_reconnects_by_itself(port: u16, opener: bool) {
    let (mut rig, down, opened) = gated_rig(port, opener);
    let peer = rig.a_peer().expect("peer");
    assert!(rig.a.world().resource::<NetLink>().resume_ticket().is_some());
    let before = rig.a_events.len();
    down.store(true, Ordering::Relaxed);
    rig.run(45 * FRAMES_PER_S);
    assert!(
        rig.a.world().resource::<NetLink>().is_reconnecting(),
        "a reconnects by itself after the timeout: {:?}",
        &rig.a_events[before..]
    );
    let status = rig.a.world().resource::<NetAgentState>().reports()["client"]
        .status
        .clone();
    assert_eq!(status.state, "reconnecting", "net.status shows the outage");
    let r = status.reconnect.expect("net.status reconnect");
    assert!(r.attempt >= 4, "{r:?}");
    assert!(r.next_in_ms <= 8_000 && r.window_left_ms > 0, "{r:?}");
    down.store(false, Ordering::Relaxed);
    let frames = rig
        .until_back(peer, 10 * FRAMES_PER_S)
        .expect("a came back as the same peer");
    assert!(
        frames <= 2 * FRAMES_PER_S,
        "back within 2 s of the link returning, took {frames} frames"
    );
    rig.run(2 * FRAMES_PER_S);
    let events = &rig.a_events[before..];
    let interrupted = rig.count(|e| matches!(e, LinkEvent::PeerInterrupted { .. }));
    let restored = rig.count(|e| matches!(e, LinkEvent::PeerRestored { .. }));
    assert_eq!((interrupted, restored), (1, 1), "once per outage: {events:?}");
    let attempts: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            LinkEvent::Reconnecting { attempt } => Some(*attempt),
            _ => None,
        })
        .collect();
    assert!(!attempts.is_empty(), "{events:?}");
    assert_eq!(
        attempts,
        (1..=attempts.len() as u32).collect::<Vec<_>>(),
        "numbered in order"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            LinkEvent::Ended { .. } | LinkEvent::ReconnectFailed { .. } | LinkEvent::Connected { .. }
        )),
        "the outage neither ended nor joined anew: {events:?}"
    );
    // Order: Interrupted, attempts, Restored.
    let first = |f: &dyn Fn(&LinkEvent) -> bool| events.iter().position(f).expect("event");
    assert!(
        first(&|e| matches!(e, LinkEvent::PeerInterrupted { .. }))
            < first(&|e| matches!(e, LinkEvent::Reconnecting { .. }))
    );
    assert!(
        first(&|e| matches!(e, LinkEvent::Reconnecting { .. }))
            < first(&|e| matches!(e, LinkEvent::PeerRestored { .. }))
    );
    assert!(!rig.a.world().resource::<NetLink>().is_reconnecting());
    assert!(
        rig.a.world().resource::<NetAgentState>().reports()["client"]
            .status
            .reconnect
            .is_none()
    );
    // The server held it dormant and took it back: same id, no leave, one resume.
    assert_eq!(rig.server.world().resource::<NetNode>().peers(), vec![peer]);
    assert!(
        !rig.server_events
            .iter()
            .any(|e| matches!(e, LinkEvent::Left { .. }))
    );
    let st = rig.server.world().resource::<NetResumeState>();
    assert_eq!((st.went_dormant, st.resumed), (1, 1));
    if opener {
        // One socket to connect, then one per attempt: every attempt a fresh socket.
        assert_eq!(opened.load(Ordering::Relaxed), 1 + attempts.len() as u32);
    } else {
        assert_eq!(opened.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn long_outage_25_s_past_the_timeout_reconnects_on_its_own_socket() {
    long_outage_reconnects_by_itself(7931, false);
}

#[test]
fn long_outage_25_s_past_the_timeout_reconnects_on_fresh_sockets() {
    long_outage_reconnects_by_itself(7932, true);
}

/// The window ends (the link never comes back): one `ReconnectFailed { reason: "timeout" }`, one
/// `Ended`, and no attempt after it.
#[test]
fn the_window_end_gives_up_once() {
    let (mut rig, down, _) = gated_rig(7933, false);
    let before = rig.a_events.len();
    down.store(true, Ordering::Relaxed);
    // 20 s timeout + 60 s window, and some margin.
    rig.run(85 * FRAMES_PER_S);
    let events = rig.a_events[before..].to_vec();
    let failed: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            LinkEvent::ReconnectFailed { attempts, reason } => Some((*attempts, reason.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(failed.len(), 1, "{events:?}");
    let attempts = events
        .iter()
        .filter(|e| matches!(e, LinkEvent::Reconnecting { .. }))
        .count();
    assert_eq!(failed[0], (attempts as u32, "timeout".to_owned()));
    // 1, 2, 4 s, then every 8 s for 60 s.
    assert!((8..=11).contains(&attempts), "{events:?}");
    let ended: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, LinkEvent::Ended { .. }))
        .collect();
    assert_eq!(
        ended,
        vec![&LinkEvent::Ended {
            reason: "timeout".into()
        }]
    );
    let failed_at = events
        .iter()
        .position(|e| matches!(e, LinkEvent::ReconnectFailed { .. }));
    let ended_at = events.iter().position(|e| matches!(e, LinkEvent::Ended { .. }));
    assert!(failed_at < ended_at, "the final event, then the end");
    assert!(!rig.a.world().resource::<NetLink>().is_reconnecting());
    // Silence after the end, even when the link comes back.
    down.store(false, Ordering::Relaxed);
    let after = rig.a_events.len();
    rig.run(30 * FRAMES_PER_S);
    assert_eq!(rig.a_events.len(), after, "{:?}", &rig.a_events[after..]);
    assert!(!rig.a.world().resource::<NetLink>().is_connected());
    assert!(
        rig.server.world().resource::<NetNode>().peers().is_empty(),
        "the grace ended"
    );
}

/// `ReconnectPolicy::off()` keeps the old behavior: the timeout ends the client.
#[test]
fn reconnect_off_ends_at_the_timeout() {
    let net = LoopbackNet::new();
    let mut server = app(NetMode::DedicatedServer);
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<ReconnectGame>(Box::new(net.bind(addr(7934)).expect("bind")), &options())
        .expect("host");
    let down = Arc::new(AtomicBool::new(false));
    let mut a = app(NetMode::Client);
    a.world_mut()
        .resource_mut::<NetLink>()
        .connect::<ReconnectGame>(
            Box::new(GateSocket {
                socket: net.bind(addr(0)).expect("bind"),
                down: down.clone(),
            }),
            addr(7934),
            &LinkOptions {
                reconnect: ReconnectPolicy::off(),
                ..options()
            },
        )
        .expect("connect");
    let mut rig = Rig::new(server, a);
    let before = rig.a_events.len();
    down.store(true, Ordering::Relaxed);
    rig.run(25 * FRAMES_PER_S);
    let events = &rig.a_events[before..];
    assert!(
        matches!(
            events,
            [
                LinkEvent::PeerInterrupted { .. },
                // ADR-8182: the grade shows the interruption once.
                LinkEvent::QualityChanged {
                    to: regia_net::LinkGrade::Interrupted,
                    ..
                },
                LinkEvent::Ended { reason },
            ] if reason == "timeout"
        ),
        "{events:?}"
    );
    assert!(!rig.a.world().resource::<NetLink>().is_reconnecting());
}

// ---- wifi-to-cell --------------------------------------------------------------------------

/// The shared virtual clock of the impaired sockets, in milliseconds (the test advances it with
/// the link's manual clock).
type Clock = Arc<AtomicU64>;

/// One radio path: one-way delay and random loss each way, seeded.
#[derive(Clone, Copy)]
struct Path {
    delay_ms: u64,
    loss: f64,
}

struct Lossy {
    state: u64,
}

impl Lossy {
    fn hit(&mut self, p: f64) -> bool {
        // xorshift64*
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        let x = self.state.wrapping_mul(0x2545_f491_4f6c_dd1d);
        ((x >> 11) as f64 / (1u64 << 53) as f64) < p
    }
}

/// A phone radio's socket: `path` each way while `alive`; dead after the switch (sends fail, as a
/// socket bound to a network that went away does, and nothing arrives).
struct RadioSocket {
    socket: LoopbackSocket,
    path: Path,
    clock: Clock,
    alive: Arc<AtomicBool>,
    rng: Lossy,
    inbound: VecDeque<(u64, Vec<u8>, SocketAddr)>,
    outbound: VecDeque<(u64, Vec<u8>, SocketAddr)>,
}

impl RadioSocket {
    fn new(socket: LoopbackSocket, path: Path, clock: Clock, alive: Arc<AtomicBool>, seed: u64) -> Self {
        Self {
            socket,
            path,
            clock,
            alive,
            rng: Lossy { state: seed | 1 },
            inbound: VecDeque::new(),
            outbound: VecDeque::new(),
        }
    }

    fn now(&self) -> u64 {
        self.clock.load(Ordering::Relaxed)
    }

    fn release_outbound(&mut self) {
        let now = self.now();
        while self.outbound.front().is_some_and(|(due, ..)| *due <= now) {
            if let Some((_, bytes, dest)) = self.outbound.pop_front() {
                let _ = self.socket.send_to(&bytes, dest);
            }
        }
    }
}

impl DatagramSocket for RadioSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if !self.alive.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::NetworkUnreachable,
                "the network went away",
            ));
        }
        if !self.rng.hit(self.path.loss) {
            self.outbound
                .push_back((self.now() + self.path.delay_ms, bytes.to_vec(), dest));
        }
        self.release_outbound();
        Ok(())
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.release_outbound();
        let mut scratch = [0u8; 2048];
        while let Some((n, from)) = self.socket.recv_from(&mut scratch)? {
            if self.alive.load(Ordering::Relaxed) && !self.rng.hit(self.path.loss) {
                self.inbound
                    .push_back((self.now() + self.path.delay_ms, scratch[..n].to_vec(), from));
            }
        }
        if !self.alive.load(Ordering::Relaxed) {
            self.inbound.clear();
            return Ok(None);
        }
        if self.inbound.front().is_some_and(|(due, ..)| *due <= self.now())
            && let Some((_, bytes, from)) = self.inbound.pop_front()
        {
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            return Ok(Some((n, from)));
        }
        Ok(None)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

const WIFI: Path = Path {
    delay_ms: 15,
    loss: 0.005,
};
const CELL: Path = Path {
    delay_ms: 60,
    loss: 0.03,
};

/// One wifi-to-cell run: 10 s on Wi-Fi, the switch (the Wi-Fi socket dies, the cell network comes
/// up 2 s later at a new address, with the RTT step and the loss change), then 20 s on cell.
/// Returns (same peer, the game's A events from the switch on, server events).
fn wifi_to_cell_run(port: u16, seed: u64) -> (bool, Vec<LinkEvent>, Vec<LinkEvent>) {
    let net = LoopbackNet::new();
    let clock: Clock = Arc::new(AtomicU64::new(0));
    let mut server = app(NetMode::DedicatedServer);
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<ReconnectGame>(Box::new(net.bind(addr(port)).expect("bind")), &options())
        .expect("host");
    let wifi_alive = Arc::new(AtomicBool::new(true));
    let cell_up = Arc::new(AtomicBool::new(false));
    let opens = Arc::new(Mutex::new(0u64));
    let open: LinkSocketOpener = {
        let (net, clock, wifi_alive, cell_up) =
            (net.clone(), clock.clone(), wifi_alive.clone(), cell_up.clone());
        Arc::new(move || {
            let n = {
                let mut o = opens.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                *o += 1;
                *o
            };
            if wifi_alive.load(Ordering::Relaxed) {
                return Ok(Box::new(RadioSocket::new(
                    net.bind(addr(0))?,
                    WIFI,
                    clock.clone(),
                    wifi_alive.clone(),
                    seed ^ n,
                )));
            }
            if !cell_up.load(Ordering::Relaxed) {
                return Err(io::Error::new(
                    io::ErrorKind::NetworkUnreachable,
                    "no network yet",
                ));
            }
            // A new address on the cell network.
            Ok(Box::new(RadioSocket::new(
                net.bind(addr(0))?,
                CELL,
                clock.clone(),
                Arc::new(AtomicBool::new(true)),
                seed.rotate_left(17) ^ n,
            )))
        })
    };
    let mut a = app(NetMode::Client);
    a.world_mut()
        .resource_mut::<NetLink>()
        .connect_via::<ReconnectGame>(open, addr(port), &options())
        .expect("connect");
    let mut server_events = Vec::new();
    let mut a_events = Vec::new();
    let mut frame = 0u64;
    let mut step = |server: &mut App, a: &mut App, a_events: &mut Vec<LinkEvent>, frame: &mut u64| {
        server.update();
        server_events.extend(server.world_mut().resource_mut::<NetLink>().take_events());
        a.update();
        a_events.extend(a.world_mut().resource_mut::<NetLink>().take_events());
        *frame += 1;
        clock.store(*frame * STEP_US / 1000, Ordering::Relaxed);
    };
    for _ in 0..10 * FRAMES_PER_S {
        step(&mut server, &mut a, &mut a_events, &mut frame);
    }
    let peer = a.world().resource::<NetLink>().status().and_then(|s| s.peer_id);
    assert!(peer.is_some(), "seed {seed}: connected on wifi: {a_events:?}");
    a_events.clear();
    wifi_alive.store(false, Ordering::Relaxed);
    for _ in 0..2 * FRAMES_PER_S {
        step(&mut server, &mut a, &mut a_events, &mut frame);
    }
    cell_up.store(true, Ordering::Relaxed);
    for _ in 0..20 * FRAMES_PER_S {
        step(&mut server, &mut a, &mut a_events, &mut frame);
    }
    let link = a.world().resource::<NetLink>();
    let back = link.is_connected() && link.status().and_then(|s| s.peer_id) == peer;
    (back, a_events, server_events)
}

/// wifi-to-cell over 20 seeded runs: 0 lost slots (every run the same `PeerId`, never a leave or a
/// second join on the server), and one Interrupted and one Restored per outage.
#[test]
fn wifi_to_cell_loses_no_slot_over_20_runs() {
    wifi_to_cell_runs(0x5eed_0000, 20);
}

/// Fifty more wifi-to-cell runs. The port base is 8100 so a parallel run of the 20-run test
/// (ports 7940+) does not share a server address inside one process if a future harness binds
/// them on one network. Each run still builds its own `LoopbackNet`.
#[test]
fn wifi_to_cell_loses_no_slot_over_50_runs() {
    wifi_to_cell_runs_at(0x50aa_0000, 50, 8100);
}

/// Evidence sweeps for ADR-8535 (a lost accept must not turn a resume into a new join): 200 runs
/// at each of four more seed bases. Run 12 of the 20-run test lost its slot that way before.
#[test]
#[ignore = "evidence sweep (ADR-8535): cargo test -p regia_net --test link_reconnect -- --ignored"]
fn wifi_to_cell_sweep_200_runs_seed_base_a() {
    wifi_to_cell_runs(0xa11c_e000_0000, 200);
}

/// See [`wifi_to_cell_sweep_200_runs_seed_base_a`].
#[test]
#[ignore = "evidence sweep (ADR-8535): cargo test -p regia_net --test link_reconnect -- --ignored"]
fn wifi_to_cell_sweep_200_runs_seed_base_b() {
    wifi_to_cell_runs(0xb0b0_0000_0000, 200);
}

/// See [`wifi_to_cell_sweep_200_runs_seed_base_a`].
#[test]
#[ignore = "evidence sweep (ADR-8535): cargo test -p regia_net --test link_reconnect -- --ignored"]
fn wifi_to_cell_sweep_200_runs_seed_base_c() {
    wifi_to_cell_runs(0xcafe_0000_0000, 200);
}

/// See [`wifi_to_cell_sweep_200_runs_seed_base_a`].
#[test]
#[ignore = "evidence sweep (ADR-8535): cargo test -p regia_net --test link_reconnect -- --ignored"]
fn wifi_to_cell_sweep_200_runs_seed_base_d() {
    wifi_to_cell_runs(0xd00d_0000_0000, 200);
}

/// `runs` wifi-to-cell runs with seeds `base + run`: no lost slot, one Interrupted and one
/// Restored per outage, never an end.
fn wifi_to_cell_runs(base: u64, runs: u16) {
    wifi_to_cell_runs_at(base, runs, 7940);
}

/// `runs` wifi-to-cell runs with seeds `base + run`, servers on `port_base + run`.
fn wifi_to_cell_runs_at(base: u64, runs: u16, port_base: u16) {
    let mut lost = Vec::new();
    for run in 0..runs {
        let seed = base + u64::from(run);
        let (back, a_events, server_events) = wifi_to_cell_run(port_base.wrapping_add(run), seed);
        let left = server_events
            .iter()
            .filter(|e| matches!(e, LinkEvent::Left { .. }))
            .count();
        let joined = server_events
            .iter()
            .filter(|e| matches!(e, LinkEvent::Joined { .. }))
            .count();
        if !back || left != 0 || joined != 1 {
            lost.push((run, back, left, joined, a_events.clone()));
            continue;
        }
        let interrupted = a_events
            .iter()
            .filter(|e| matches!(e, LinkEvent::PeerInterrupted { .. }))
            .count();
        let restored = a_events
            .iter()
            .filter(|e| matches!(e, LinkEvent::PeerRestored { .. }))
            .count();
        assert_eq!(
            (interrupted, restored),
            (1, 1),
            "seed {seed:#x}: once per outage: {a_events:?}"
        );
        assert!(
            !a_events
                .iter()
                .any(|e| matches!(e, LinkEvent::Ended { .. } | LinkEvent::ReconnectFailed { .. })),
            "seed {seed:#x}: {a_events:?}"
        );
    }
    assert!(lost.is_empty(), "seed base {base:#x}: lost slots: {lost:?}");
}
