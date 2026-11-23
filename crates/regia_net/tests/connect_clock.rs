// SPDX-License-Identifier: MPL-2.0
//! The client's server-tick estimate from the moment it connects (spec 120 § 9.1, ADR-10096).
//!
//! The accept carries the server's tick but no delay, and the connection's first round-trip
//! sample is one round trip away: until it came back the estimate used to sit the whole one-way
//! delay behind the server (45 ticks at 30 Hz on spec 120's `rtt-extreme` link, for three
//! seconds), and every input of that stretch was stamped for a tick long past. The handshake has
//! already crossed the path two or three times, so the client anchors the clock with the
//! handshake's shortest round trip.
//!
//! One client and a 30 Hz server over the loopback net with the crate's conditioner on both
//! directions, 1 ms steps, seeded, virtual time (no wall clock, no sleeps). The client runs a
//! frame every 16 ms and the server reads its socket once a tick, like a paced host.

use std::net::SocketAddr;

use regia_net::prelude::*;
use regia_net::{CryptoMode, LoopbackSocket};

struct Game;
impl NetGame for Game {
    type Message = Vec<u8>;
    type Input = i32;
    type JoinData = ();
    const GAME_ID: &'static str = "test.connect-clock";
    const PROTOCOL_VERSION: u32 = 1;
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

const SERVER: u16 = 7777;
const TICK_HZ: u32 = 30;
const FRAME_MS: u64 = 16;

fn conditioner(seed: u64, delay: u64, jitter: u64, loss: f64) -> ConditionerConfig {
    ConditionerConfig {
        enabled: true,
        seed,
        loss,
        delay_ms: delay,
        jitter_ms: jitter,
        ..ConditionerConfig::default()
    }
}

/// What the estimate did after the client connected.
#[derive(Debug, Default)]
struct Outcome {
    /// When the client connected, virtual milliseconds (`None`: it never did).
    connected_ms: Option<u64>,
    /// The estimate minus the server's tick at the first frame after connecting.
    first: i64,
    /// The most ticks the estimate was behind the server's tick in the first two round trips.
    behind: i64,
    /// The most ticks it was ahead in the first two round trips.
    ahead: i64,
    /// The most it was off, either way, after that.
    settled: i64,
    /// Forward steps of the estimate over 8 ticks.
    resyncs: u64,
    /// How long after the connect the server first ran a tick with this client's own input for
    /// it in time (`TickDriver::missed` did not count the tick), milliseconds (`None`: never in
    /// the run).
    first_input_ms: Option<u64>,
    /// Ticks the server ran without the client's input in time after that first one.
    missed_after_first: u64,
    /// When the first of them were, milliseconds after the connect (at most 24).
    missed_at: Vec<u64>,
    /// The largest input lead the client used, ticks.
    lead_max: u64,
    /// Every change of the lead: `(milliseconds after the connect, ticks)` (at most 16).
    lead_at: Vec<(u64, u64)>,
}

/// A path of `delay` ms one way, up to `jitter` ms extra and `loss` random loss per direction.
type Path = (u64, u64, f64);

type Server = NetServer<Game, LoopbackSocket>;
type Client = NetClient<Game, LoopbackSocket>;

/// A server at `hz` ticks a second whose sends cross `path`.
fn host(net: &LoopbackNet, hz: u32, (delay, jitter, loss): Path, crypto: CryptoMode, seed: u64) -> Server {
    host_with(net, hz, (delay, jitter, loss), crypto, seed, |_| {})
}

/// [`host`] with its configuration edited.
fn host_with(
    net: &LoopbackNet,
    hz: u32,
    (delay, jitter, loss): Path,
    crypto: CryptoMode,
    seed: u64,
    edit: impl FnOnce(&mut ServerConfig),
) -> Server {
    let mut config = ServerConfig {
        crypto,
        tick_rate_hz: hz,
        conditioner: conditioner(seed.wrapping_mul(2) + 1, delay, jitter, loss),
        ..ServerConfig::default()
    };
    edit(&mut config);
    NetServer::with_secret(net.bind(addr(SERVER)).expect("bind"), config, [7; 32])
}

/// A client whose sends cross `path`, connecting.
fn connect(
    net: &LoopbackNet,
    (delay, jitter, loss): Path,
    crypto: CryptoMode,
    seed: u64,
    edit: impl FnOnce(&mut ClientConfig),
) -> Client {
    let mut config = ClientConfig {
        crypto,
        conditioner: conditioner(seed.wrapping_mul(2) + 2, delay, jitter, loss),
        // The slowest link here needs several handshake legs of 3 s each.
        connect_timeout_ms: 60_000,
        ..ClientConfig::default()
    };
    edit(&mut config);
    NetClient::connect(net.bind(addr(0)).expect("bind"), addr(SERVER), config, &()).expect("connect")
}

/// One client against a 30 Hz server on a symmetric path, until `after_ms` past the connect. The
/// client skips its frames inside `stall` (a first frame that compiles for seconds).
fn run(path: Path, crypto: CryptoMode, stall: Option<(u64, u64)>, after_ms: u64, seed: u64) -> Outcome {
    run_at(TICK_HZ, path, crypto, stall, after_ms, seed)
}

/// [`run`] against a server at `hz` ticks a second.
fn run_at(
    hz: u32,
    path: Path,
    crypto: CryptoMode,
    stall: Option<(u64, u64)>,
    after_ms: u64,
    seed: u64,
) -> Outcome {
    let net = LoopbackNet::new();
    let mut server = host(&net, hz, path, crypto, seed);
    let mut client = connect(&net, path, crypto, seed, |_| {});
    let (out, _) = drive(&mut server, &mut client, hz, 0, path, stall, after_ms);
    println!("connect-clock hz={hz} path={path:?} crypto={crypto:?} stall={stall:?} seed={seed}: {out:?}");
    out
}

/// Steps `client` (a frame every 16 ms) and `server` (its socket read once a tick) in 1 ms steps
/// from `start` until `after_ms` past the client's connect. Returns what the estimate did and
/// the clock at the end.
fn drive(
    server: &mut Server,
    client: &mut Client,
    hz: u32,
    start: u64,
    (delay, jitter, _): Path,
    stall: Option<(u64, u64)>,
    after_ms: u64,
) -> (Outcome, u64) {
    let mut ticker = TickClock::new(hz, 8);
    let mut out = Outcome::default();
    let round_trips = 2 * (2 * delay + 2 * jitter) + 500;
    let mut now = start;
    loop {
        let stalled = stall.is_some_and(|(from, to)| now >= from && now < to);
        if now.is_multiple_of(FRAME_MS) && !stalled {
            client.receive(now);
            while let Some(event) = client.poll_event() {
                if let ClientEvent::Connected { .. } = event {
                    out.connected_ms = Some(now);
                    out.first = client.estimated_server_tick(now) as i64 - server.driver().tick() as i64;
                }
            }
            if let Some(at) = out.connected_ms {
                let off = client.estimated_server_tick(now) as i64 - server.driver().tick() as i64;
                if now < at + round_trips {
                    out.behind = out.behind.max(-off);
                    out.ahead = out.ahead.max(off);
                } else {
                    out.settled = out.settled.max(off.abs());
                }
                let lead = client.input_lead_ticks();
                out.lead_max = out.lead_max.max(lead);
                if out.lead_at.last().is_none_or(|&(_, l)| l != lead) && out.lead_at.len() < 16 {
                    out.lead_at.push((now - at, lead));
                }
                for tick in client.due_input_ticks(now) {
                    let _ = client.send_input(tick, tick as i32);
                }
            }
            client.flush(now);
        }
        let ticks = ticker.advance(1);
        if ticks > 0 {
            server.receive(now);
            while server.poll_event().is_some() {}
            for _ in 0..ticks {
                let before = client.peer_id().map(|p| server.driver().missed(p));
                server.advance_tick(|_, _| {});
                if let (Some(p), Some(at), Some(before)) = (client.peer_id(), out.connected_ms, before) {
                    if server.driver().missed(p) == before {
                        out.first_input_ms.get_or_insert(now - at);
                    } else if out.first_input_ms.is_some() {
                        out.missed_after_first += 1;
                        if out.missed_at.len() < 24 {
                            out.missed_at.push(now - at);
                        }
                    }
                }
            }
            server.flush(now);
        }
        now += 1;
        match out.connected_ms {
            Some(at) if now >= at + after_ms => break,
            None if now >= start + 120_000 => break,
            _ => {}
        }
    }
    out.resyncs = client.clock_resyncs();
    (out, now)
}

/// Spec 120's `rtt-extreme` link without its loss: 1,500 ms one way, 45 ticks at 30 Hz. From the
/// first frame after the accept the estimate is within a tick and a half of the server's tick
/// (never ahead of it), and the first round-trip sample finds nothing to step over.
#[test]
fn the_estimate_is_on_the_server_tick_from_the_accept_at_1500_ms_one_way() {
    for crypto in [CryptoMode::Required, CryptoMode::Off] {
        let o = run((1_500, 0, 0.0), crypto, None, 12_000, 1);
        assert!(o.connected_ms.is_some(), "{crypto:?}: connects: {o:?}");
        assert!(
            (-2..=0).contains(&o.first),
            "{crypto:?}: at the accept the estimate is at most 2 ticks behind the server's tick and not ahead: {o:?}"
        );
        assert!(
            o.behind <= 2 && o.ahead <= 1,
            "{crypto:?}: it stays there through the first round trips: {o:?}"
        );
        assert!(o.settled <= 1, "{crypto:?}: and after them: {o:?}");
        assert_eq!(o.resyncs, 0, "{crypto:?}: no forward step: {o:?}");
        // The estimate is the server's tick now and the lead covers the way there. Before the
        // server's first feedback the lead already holds the handshake's one-way delay, so the
        // first input the client stamps reaches its tick: the server has this client's input in
        // time one one-way delay after the connect (plus a client frame, the server's tick and
        // the tick the stamp rounds up to). It used to be late until the feedback had come back
        // and raised the lead: a round trip and a one-way delay, 4.5 s here.
        assert!(
            o.first_input_ms.is_some_and(|ms| ms <= 1_500 + 100),
            "{crypto:?}: inputs are in time one one-way delay after the connect: {o:?}"
        );
        assert!(
            o.lead_at
                .first()
                .is_some_and(|&(_, lead)| (45..=45 + 3).contains(&lead)),
            "{crypto:?}: the first lead is the path's 45 ticks and its margin: {o:?}"
        );
        assert!(
            o.lead_max <= 45 + 6,
            "{crypto:?}: the lead is the path plus its margin: {o:?}"
        );
        assert!(
            o.missed_after_first <= 2,
            "{crypto:?}: and they stay in time: {o:?}"
        );
    }
}

/// `high-rtt`: 325 ms one way, up to 20 ms jitter, 1 % loss. In milliseconds the anchor can be
/// behind by the frame the accept waited for (up to 16), the accept's own jitter (up to 20),
/// half of what the shortest leg under-reads (it ends at the poll before the reply was read: up
/// to 8) and half a tick of the server's wait: 61 ms, 1.8 ticks, and one more for counting whole
/// ticks. It was 10 to 13 ticks behind.
#[test]
fn the_estimate_is_on_the_server_tick_from_the_accept_on_high_rtt() {
    for seed in 1..=8 {
        let o = run((325, 20, 0.01), CryptoMode::Required, None, 8_000, seed);
        assert!(o.connected_ms.is_some(), "seed {seed}: connects: {o:?}");
        assert!(
            (-3..=1).contains(&o.first),
            "seed {seed}: at the accept the estimate is within 3 ticks of the server's tick: {o:?}"
        );
        assert!(o.behind <= 3 && o.ahead <= 2, "seed {seed}: {o:?}");
        assert_eq!(o.resyncs, 0, "seed {seed}: {o:?}");
        // The first lead covers the path (345 ms at most is 11 ticks, and the margin of two), so
        // the first input the client stamps is in time: the server reaches that tick one one-way
        // delay after the connect, plus a frame and the three ticks of margin and rounding. It
        // used to take the feedback loop, a round trip longer.
        assert!(
            o.lead_at
                .first()
                .is_some_and(|&(_, lead)| (10..=13).contains(&lead)),
            "seed {seed}: the first lead is the path and its margin: {o:?}"
        );
        assert!(
            o.first_input_ms.is_some_and(|ms| ms <= 345 + 16 + 100),
            "seed {seed}: inputs are in time one one-way delay after the connect: {o:?}"
        );
    }
}

/// A lost first copy makes a handshake leg longer than the path by whole resend intervals
/// (250 ms: 3.75 ticks one way at 30 Hz). The connect keeps its shortest leg, so the estimate is
/// ahead only when every leg lost one: on `rtt-extreme`'s 20 % loss it is never far ahead, and it
/// is never the one-way delay behind.
#[test]
fn lost_handshake_copies_do_not_carry_the_estimate_away() {
    let mut worst = (0, 0);
    for seed in 1..=12 {
        let o = run((1_500, 0, 0.2), CryptoMode::Required, None, 15_000, seed);
        assert!(o.connected_ms.is_some(), "seed {seed}: connects: {o:?}");
        worst = (worst.0.max(o.behind), worst.1.max(o.ahead));
        assert!(
            o.behind <= 2,
            "seed {seed}: never the one-way delay behind (45 ticks): {o:?}"
        );
        assert!(
            o.ahead <= 8,
            "seed {seed}: at most two resend intervals ahead: {o:?}"
        );
        assert!(o.settled <= 2, "seed {seed}: settles on the server's tick: {o:?}");
    }
    println!(
        "connect-clock 20 % loss, 12 seeds: worst behind {} ahead {}",
        worst.0, worst.1
    );
}

/// A client whose frames stall for two seconds with the server's answer waiting in its socket
/// (a first frame that compiles shaders, ADR-3075) reads no delay into the stall: the reply was
/// not there at the poll before, and that is all a leg counts.
#[test]
fn a_stalled_client_reads_no_delay_into_its_stall() {
    for stall in [(40, 2_040), (100, 2_100), (160, 2_160)] {
        let o = run((15, 0, 0.0), CryptoMode::Required, Some(stall), 4_000, 3);
        assert!(o.connected_ms.is_some(), "stall {stall:?}: connects: {o:?}");
        assert!(
            o.behind <= 2 && o.ahead <= 1,
            "stall {stall:?}: the estimate is on the server's tick: {o:?}"
        );
        assert_eq!(o.resyncs, 0, "stall {stall:?}: {o:?}");
    }
}

/// A fast link is where it was: within a tick of the server's tick from the accept, and its
/// lead starts where it did. The server's wait for its next tick is in every round-trip sample
/// and in the lead's two ticks already, so the startup lead does not count it twice: two ticks
/// on a link without delay, and one more per tick of path.
#[test]
fn a_fast_link_is_on_the_server_tick_from_the_accept() {
    for (delay, jitter, first_lead, lead_max) in [(0, 0, 2..=2, 2), (15, 5, 2..=3, 4), (60, 10, 4..=5, 6)] {
        let o = run((delay, jitter, 0.0), CryptoMode::Required, None, 4_000, 5);
        assert!(o.connected_ms.is_some(), "{delay} ms: connects: {o:?}");
        assert!(
            o.behind <= 2 && o.ahead <= 1,
            "{delay} ms: the estimate is on the server's tick: {o:?}"
        );
        assert_eq!(o.resyncs, 0, "{delay} ms: {o:?}");
        assert!(
            o.lead_at
                .first()
                .is_some_and(|(_, lead)| first_lead.contains(lead)),
            "{delay} ms: the first lead is two ticks and the path's: {o:?}"
        );
        assert!(
            o.lead_max <= lead_max,
            "{delay} ms: and the feedback finds little to add: {o:?}"
        );
        assert_eq!(o.missed_after_first, 0, "{delay} ms: no input is late: {o:?}");
    }
}

/// A rejoin that holds the resumption credential of the link before opens with the resume flight
/// in place of the connect request (wire 6; a server that does not take the flight answers it
/// with a challenge and the full handshake follows). Whichever way the server answers, the legs
/// are timed the same way: the estimate is on the server's tick from the accept.
#[test]
fn a_rejoin_with_a_resume_credential_is_on_the_server_tick_from_the_accept() {
    for seed in 1..=4 {
        let path = (325, 20, 0.0);
        let net = LoopbackNet::new();
        // A server that keeps a leaver's seat for a while hands out resume tickets.
        let mut server = host_with(&net, TICK_HZ, path, CryptoMode::Required, seed, |config| {
            config.resume_grace_ms = 30_000;
        });
        let mut first = connect(&net, path, CryptoMode::Required, seed, |_| {});
        let (o, now) = drive(&mut server, &mut first, TICK_HZ, 0, path, None, 2_000);
        assert!(o.connected_ms.is_some(), "seed {seed}: connects: {o:?}");
        let ticket = first.resume_ticket().expect("a resume ticket");
        let credential = first.resumption().expect("a sealed accept carries a credential");
        // The first connection just goes quiet; the same player comes back from another port.
        drop(first);
        let mut back = connect(&net, path, CryptoMode::Required, seed + 100, |config| {
            config.resume = Some(ticket);
            config.resumption = Some(credential);
        });
        let (o, _) = drive(&mut server, &mut back, TICK_HZ, now, path, None, 8_000);
        println!("connect-clock rejoin path={path:?} seed={seed}: {o:?}");
        assert!(o.connected_ms.is_some(), "seed {seed}: rejoins: {o:?}");
        assert_eq!(
            back.totals().resume_inits,
            1,
            "seed {seed}: the rejoin opened with the resume flight"
        );
        assert!(
            (-3..=1).contains(&o.first),
            "seed {seed}: at the accept the estimate is within 3 ticks of the server's tick: {o:?}"
        );
        assert!(o.behind <= 3 && o.ahead <= 2, "seed {seed}: {o:?}");
        assert_eq!(o.resyncs, 0, "seed {seed}: {o:?}");
    }
}

/// The same at 64 and 128 Hz, where a tick is 15.6 and 7.8 ms and the client's 16 ms frame is
/// one or two ticks. In milliseconds the anchor can be behind by: the frame the accept waited
/// for (up to 16), half of what the leg under-reads (it ends at the poll before the reply was
/// read: up to 8) and half a tick of the server's wait the one tick taken off did not cover.
/// That is 2.1 ticks at 64 Hz and 3.6 at 128 Hz, plus one for counting whole ticks and the
/// path's jitter. The whole one-way delay it used to be behind is 21 and 42 ticks at 325 ms.
/// (A path longer than the server's 64-tick input buffer, 1,000 ms at 64 Hz and 500 ms at
/// 128 Hz, is outside this test: the estimate is held inside the buffer by design.)
#[test]
fn the_estimate_is_on_the_server_tick_from_the_accept_at_64_and_128_hz() {
    for (hz, frame_ticks) in [(64u32, 3i64), (128, 4)] {
        for (path, after_ms) in [
            ((325, 20, 0.0), 6_000),
            ((60, 10, 0.0), 3_000),
            ((15, 5, 0.0), 3_000),
        ] {
            let o = run_at(hz, path, CryptoMode::Required, None, after_ms, 2);
            assert!(o.connected_ms.is_some(), "{hz} Hz {path:?}: connects: {o:?}");
            let jitter_ticks = (path.1 * u64::from(hz)).div_ceil(1000) as i64;
            let bound = frame_ticks + 1 + jitter_ticks;
            assert!(
                o.behind <= bound && o.ahead <= bound,
                "{hz} Hz {path:?}: the estimate is within {bound} ticks of the server's tick from the accept: {o:?}"
            );
            assert_eq!(o.resyncs, 0, "{hz} Hz {path:?}: {o:?}");
        }
    }
}
