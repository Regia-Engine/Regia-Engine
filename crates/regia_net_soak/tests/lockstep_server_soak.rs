// SPDX-License-Identifier: MPL-2.0
//! The lockstep server game type (`LockstepServer`, the 40-seat core of ADR-8558) on the soak kit's
//! impaired links (ADR-8656): `bad-wifi`, `mobile-4g` (with its NAT rebinding), `lockstep-mix`
//! and `hostile`, 40 seats, the observer voting, in virtual time (a millisecond counter; no
//! sleeps, no wall clock).
//!
//! Run on the ECS-free core: `cargo test -p regia_net_soak --no-default-features --test
//! lockstep_server_soak` (the default build runs the same tests).
//!
//! What the harness models. Each seat is a `LockstepSeat` behind an `ImpairedNet` endpoint with
//! the profile's access link for that seat (latency, jitter, random and Gilbert-Elliott burst
//! loss, reorder, duplication, bandwidth, MTU, outages, rebinding). The lockstep datagram lane
//! rides the impaired network as one UDP payload per datagram. The stream lane is the transport's
//! reliable channel: it is delivered once and in order, after the direction's latency plus its
//! largest jitter, and not before an outage it was sent in ends. The server maps a datagram to a
//! seat by the address the seat's endpoint holds, as an authenticated transport does; the seat a
//! payload is handed in for is the only thing that says whose it is.
//!
//! Every test checks: the honest seats step every frame in order and agree on the state hash at
//! their common tick, with no desync verdict; frames are built on the clock; a seat that goes
//! silent is removed by the timeout policy (never early) and frames keep being built after; and
//! every input a frame lists under a seat is one that seat submitted (each honest seat tags its
//! inputs with its own id). The hostile test adds a seat that replays seat 0's datagrams as its
//! own, floods duplicates, and sends garbage, truncated datagrams and garbage stream payloads: the
//! replayed inputs land on the replaying seat only, never on seat 0, and no honest seat's delay,
//! hashes or frames move.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;

use regia_net::DatagramSocket;
use regia_net::lockstep::server::{LockstepServer, LockstepServerSettings, LockstepServerStatus};
use regia_net::lockstep::{HostEvent, Lane, LockstepSeat, SeatId, SeatInput};
use regia_net_soak::impair::{EndpointId, ImpairedNet, ImpairedSocket, LinkSpec, SoakRng};
use regia_net_soak::lockstep::{InputMix, InputSource, LockGame, LockInput, LockSim};
use regia_net_soak::profiles::Profile;

/// A seat's application frame, virtual milliseconds.
const FRAME_MS: u64 = 16;
/// The first byte of an honest seat's input command: the next two are its seat id.
const HONEST_TAG: u8 = 0x5e;
/// The lane byte in front of a lockstep datagram on the impaired network.
const DATAGRAM: u8 = 0;

fn server_addr() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 1], 7_778))
}

fn seat_addr(i: usize) -> SocketAddr {
    SocketAddr::from(([10, 2 + (i / 250) as u8, 0, (i % 250) as u8 + 1], 41_000))
}

/// The command an honest seat tags every input with.
fn tag(seat: u16) -> Vec<u8> {
    vec![HONEST_TAG, seat as u8, (seat >> 8) as u8]
}

/// One direction of the transport's reliable stream on a seat's access link.
#[derive(Default)]
struct StreamPipe {
    queue: VecDeque<(u64, Vec<u8>)>,
    last_ms: u64,
}

impl StreamPipe {
    fn send(&mut self, link: &LinkSpec, up: bool, now: u64, bytes: Vec<u8>) {
        let (u, d) = link.directions_at(now);
        let dir = if up { u } else { d };
        let mut at = now + dir.latency_ms + dir.jitter_ms;
        if let Some(end) = link.outage_end(now) {
            at = at.max(end + dir.latency_ms);
        }
        at = at.max(self.last_ms);
        self.last_ms = at;
        self.queue.push_back((at, bytes));
    }

    fn due(&mut self, now: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while self.queue.front().is_some_and(|(at, _)| *at <= now) {
            if let Some((_, bytes)) = self.queue.pop_front() {
                out.push(bytes);
            }
        }
        out
    }
}

/// One seat of the match.
struct Node {
    id: SeatId,
    ep: EndpointId,
    sock: ImpairedSocket,
    link: LinkSpec,
    core: LockstepSeat<LockGame>,
    sim: LockSim,
    inputs: InputSource,
    /// The seat's state hash after each tick it stepped.
    hashes: BTreeMap<u64, u64>,
    phase: u64,
    first_sent: bool,
    in_order: bool,
    /// From this virtual millisecond on the seat sends nothing (it still receives).
    silent_from: Option<u64>,
    /// The last virtual millisecond the seat sent anything.
    last_sent_ms: u64,
    /// The replayer: no simulation, it sends seat 0's datagrams as its own plus garbage.
    hostile: bool,
    up: StreamPipe,
    down: StreamPipe,
    /// Host payloads the seat refused.
    refused: u64,
}

impl Node {
    fn silent(&self, now: u64) -> bool {
        self.silent_from.is_some_and(|t| now >= t)
    }

    fn submit(&mut self) {
        let mut input: LockInput = self.inputs.next_input();
        input.command = tag(self.id.0);
        let _ = self.core.submit_input(&input);
    }
}

/// One run.
struct Plan {
    profile: &'static str,
    seats: usize,
    seed: u64,
    duration_ms: u64,
    /// `(seat, ms)`: that honest seat stops sending at `ms`.
    silent: Option<(usize, u64)>,
    /// `(seat, ms)`: that seat is the replayer, and stops sending at `ms`.
    hostile: Option<(usize, u64)>,
}

/// What one run measured.
struct Outcome {
    status: LockstepServerStatus,
    timeout_ms: u64,
    initial_delay: u16,
    max_delay: u16,
    /// The largest frame delay a frame was built under.
    delay_max: u16,
    /// `(seat, tick, reason, virtual ms)` of every removal.
    removed: Vec<(u16, u64, &'static str, u64)>,
    /// The last virtual millisecond each seat sent anything.
    last_sent: BTreeMap<u16, u64>,
    /// Frames built by the time of each removal, by seat.
    frames_at_removal: BTreeMap<u16, u64>,
    /// An input a frame listed under a seat that did not submit it.
    foreign: Vec<String>,
    /// Inputs replayed from seat 0 that frames listed under the replayer.
    replays_landed: u64,
    /// Honest seat payloads the server refused, by error.
    honest_refused: BTreeMap<String, u64>,
    /// The replayer's payloads the server refused.
    hostile_refused: u64,
    hostile_sent: u64,
    /// Datagrams from an address that is no seat's.
    unknown_source: u64,
    /// `(seat, next tick)` of every honest seat that stayed.
    ticks: Vec<(u16, u64)>,
    /// The honest seats that stayed agree on the hash at their common tick (`None`: none stepped).
    common: Option<(u64, bool)>,
    seats_in_order: bool,
    seat_refused: u64,
}

fn run(plan: &Plan) -> Outcome {
    let profile = Profile::named(plan.profile).expect("a soak profile");
    let net = ImpairedNet::new(plan.seed);
    let server_ep = net
        .add_endpoint(server_addr(), profile.server_link(false))
        .expect("server endpoint");
    let mut server_sock = net.socket(server_ep);
    let settings = LockstepServerSettings {
        enabled: true,
        ..LockstepServerSettings::default()
    };
    let mut server =
        LockstepServer::<LockGame>::new(&settings, 60, Some(Box::new(LockSim::default()))).expect("server");
    let mut nodes = Vec::with_capacity(plan.seats);
    for i in 0..plan.seats {
        let id = SeatId(i as u16);
        server.add_seat(id).expect("a free seat");
        let link = profile.client_link(i, plan.duration_ms);
        let ep = net
            .add_endpoint(seat_addr(i), link.clone())
            .expect("seat endpoint");
        let hostile = plan.hostile.is_some_and(|(h, _)| h == i);
        let silent_from = match (plan.silent, plan.hostile) {
            (Some((s, at)), _) if s == i => Some(at),
            (_, Some((h, at))) if h == i => Some(at),
            _ => None,
        };
        nodes.push(Node {
            id,
            ep,
            sock: net.socket(ep),
            link,
            core: LockstepSeat::new(server.config().clone(), id),
            sim: LockSim::default(),
            inputs: InputSource::new(plan.seed, id.0, InputMix::ORDERS),
            hashes: BTreeMap::new(),
            phase: (i as u64 * 5) % FRAME_MS,
            first_sent: false,
            in_order: true,
            silent_from,
            last_sent_ms: 0,
            hostile,
            up: StreamPipe::default(),
            down: StreamPipe::default(),
            refused: 0,
        });
    }
    server.start(0);
    let config = server.config().clone();
    let mut out = Outcome {
        status: LockstepServerStatus::default(),
        timeout_ms: config.timeout.timeout_ms,
        initial_delay: config.initial_delay_ticks,
        max_delay: config.max_delay_ticks,
        delay_max: 0,
        removed: Vec::new(),
        last_sent: BTreeMap::new(),
        frames_at_removal: BTreeMap::new(),
        foreign: Vec::new(),
        replays_landed: 0,
        honest_refused: BTreeMap::new(),
        hostile_refused: 0,
        hostile_sent: 0,
        unknown_source: 0,
        ticks: Vec::new(),
        common: None,
        seats_in_order: true,
        seat_refused: 0,
    };
    let hostile_seat = plan.hostile.map(|(h, _)| h as u16);
    let mut by_addr: BTreeMap<SocketAddr, usize> = BTreeMap::new();
    let mut seat0_sent: Vec<Vec<u8>> = Vec::new();
    let mut rng = SoakRng::derive(plan.seed, 0x6a7b);
    let mut frames_built = 0u64;
    let mut buf = vec![0u8; 65_536];
    let mut now = 0u64;
    while now <= plan.duration_ms {
        net.advance_to_ms(now);
        // Seats, each on its own frame phase.
        for node in &mut nodes {
            if now % FRAME_MS != node.phase {
                continue;
            }
            while let Ok(Some((n, _))) = node.sock.recv_from(&mut buf) {
                if node.hostile || n < 2 || buf[0] != DATAGRAM {
                    continue;
                }
                if node.core.receive(Lane::Datagram, &buf[1..n], now).is_err() {
                    node.refused += 1;
                }
            }
            for bytes in node.down.due(now) {
                if !node.hostile && node.core.receive(Lane::Stream, &bytes, now).is_err() {
                    node.refused += 1;
                }
            }
            if node.hostile {
                if node.silent(now) {
                    continue;
                }
                node.last_sent_ms = now;
                // Seat 0's newest datagrams, sent as this seat's own, twice each; a truncated
                // copy; a garbage datagram; now and then a garbage stream payload.
                for d in &seat0_sent {
                    for _ in 0..2 {
                        let mut wire = vec![DATAGRAM];
                        wire.extend_from_slice(d);
                        let _ = node.sock.send_to(&wire, server_addr());
                        out.hostile_sent += 1;
                    }
                    let mut cut = vec![DATAGRAM];
                    cut.extend_from_slice(&d[..d.len() / 2]);
                    let _ = node.sock.send_to(&cut, server_addr());
                    out.hostile_sent += 1;
                }
                let len = 1 + rng.up_to(200) as usize;
                let mut junk = vec![DATAGRAM];
                junk.extend((0..len).map(|_| rng.next_u64() as u8));
                let _ = node.sock.send_to(&junk, server_addr());
                out.hostile_sent += 1;
                if rng.up_to(7) == 0 {
                    let junk: Vec<u8> = (0..1 + rng.up_to(64)).map(|_| rng.next_u64() as u8).collect();
                    node.up.send(&node.link, true, now, junk);
                    out.hostile_sent += 1;
                }
                continue;
            }
            if !node.first_sent {
                node.first_sent = true;
                node.submit();
            }
            while let Some(frame) = node.core.next_frame() {
                if frame.tick != node.sim.tick {
                    node.in_order = false;
                }
                node.sim.step(&frame);
                let hash = node.sim.hash();
                node.hashes.insert(frame.tick, hash);
                node.core.report_hash(frame.tick, hash);
                node.submit();
            }
            while node.core.poll_event().is_some() {}
            if node.silent(now) {
                continue;
            }
            let outgoing = node.core.outgoing(now);
            if !outgoing.is_empty() {
                node.last_sent_ms = now;
            }
            if node.id.0 == 0 {
                seat0_sent.clear();
            }
            for o in outgoing {
                match o.lane {
                    Lane::Datagram => {
                        if node.id.0 == 0 {
                            seat0_sent.push(o.bytes.clone());
                        }
                        let mut wire = vec![DATAGRAM];
                        wire.extend_from_slice(&o.bytes);
                        let _ = node.sock.send_to(&wire, server_addr());
                    }
                    Lane::Stream => node.up.send(&node.link, true, now, o.bytes),
                }
            }
        }
        // The server: what arrived, the frames due, what to send.
        while let Ok(Some((n, from))) = server_sock.recv_from(&mut buf) {
            if !by_addr.contains_key(&from) {
                for (i, node) in nodes.iter().enumerate() {
                    for a in net.addresses_held(node.ep) {
                        by_addr.insert(a, i);
                    }
                    by_addr.insert(net.public_addr(node.ep), i);
                }
            }
            let Some(&i) = by_addr.get(&from) else {
                out.unknown_source += 1;
                continue;
            };
            if n < 1 || buf[0] != DATAGRAM {
                continue;
            }
            let result = server.receive(nodes[i].id, Lane::Datagram, &buf[1..n], now);
            if let Err(e) = result {
                if nodes[i].hostile {
                    out.hostile_refused += 1;
                } else {
                    *out.honest_refused.entry(e.to_string()).or_default() += 1;
                }
            }
        }
        for node in &mut nodes {
            for bytes in node.up.due(now) {
                if let Err(e) = server.receive(node.id, Lane::Stream, &bytes, now) {
                    if node.hostile {
                        out.hostile_refused += 1;
                    } else {
                        *out.honest_refused.entry(e.to_string()).or_default() += 1;
                    }
                }
            }
        }
        let built = server.tick(now);
        server.sync_observer();
        for frame in &built {
            frames_built += 1;
            out.delay_max = out.delay_max.max(frame.delay);
            for (seat, input) in &frame.inputs {
                let (SeatInput::Present(i) | SeatInput::Held(i)) = input else {
                    continue;
                };
                if Some(seat.0) == hostile_seat {
                    if i.command == tag(0) {
                        out.replays_landed += 1;
                    }
                } else if i.command != tag(seat.0) {
                    out.foreign.push(format!(
                        "tick {} seat {}: command {:?}",
                        frame.tick, seat.0, i.command
                    ));
                }
            }
        }
        while let Some(event) = server.poll_event() {
            if let HostEvent::SeatRemoved { seat, tick, reason } = event {
                out.removed.push((seat.0, tick, reason, now));
                out.frames_at_removal.insert(seat.0, frames_built);
            }
        }
        for (to, o) in server.outgoing(now) {
            let Some(node) = nodes.get_mut(usize::from(to.0)) else {
                continue;
            };
            match o.lane {
                Lane::Datagram => {
                    let mut wire = vec![DATAGRAM];
                    wire.extend_from_slice(&o.bytes);
                    let _ = server_sock.send_to(&wire, net.public_addr(node.ep));
                }
                Lane::Stream => node.down.send(&node.link, false, now, o.bytes),
            }
        }
        now += 1;
    }
    out.status = server.status(now);
    out.last_sent = nodes.iter().map(|n| (n.id.0, n.last_sent_ms)).collect();
    let stayed: Vec<&Node> = nodes
        .iter()
        .filter(|n| !n.hostile && n.silent_from.is_none())
        .collect();
    out.ticks = stayed.iter().map(|n| (n.id.0, n.sim.tick)).collect();
    out.seats_in_order = stayed.iter().all(|n| n.in_order);
    out.seat_refused = stayed.iter().map(|n| n.refused).sum();
    out.common = stayed
        .iter()
        .map(|n| n.sim.tick)
        .min()
        .and_then(|t| t.checked_sub(1))
        .map(|tick| {
            let first = stayed.first().and_then(|n| n.hashes.get(&tick));
            (tick, stayed.iter().all(|n| n.hashes.get(&tick) == first))
        });
    out
}

/// The checks every profile must pass.
fn assert_healthy(label: &str, plan: &Plan, o: &Outcome) {
    eprintln!(
        "{label}: frames {} delay_max {} (initial {}) agreed {} verdicts {} removed {:?} replays_landed {} \
         hostile sent/refused {}/{} honest_refused {:?} seat_refused {} unknown {} common {:?}",
        o.status.frames_built,
        o.delay_max,
        o.initial_delay,
        o.status.lockstep.checkpoints_agreed,
        o.status.verdicts,
        o.removed,
        o.replays_landed,
        o.hostile_sent,
        o.hostile_refused,
        o.honest_refused,
        o.seat_refused,
        o.unknown_source,
        o.common
    );
    // Frames are built on the clock: one per tick of the run, give or take the last.
    let ticks_due = plan.duration_ms * 60 / 1_000;
    assert!(
        o.status.frames_built + 2 >= ticks_due,
        "{label}: {} frames in {} ms",
        o.status.frames_built,
        plan.duration_ms
    );
    // The honest seats that stayed step every frame in order and agree.
    assert!(o.seats_in_order, "{label}: a seat stepped out of order");
    let (common, agree) = o.common.expect("the seats stepped");
    assert!(agree, "{label}: the seats disagree at tick {common}");
    for &(seat, next) in &o.ticks {
        assert!(
            next + 60 >= o.status.frames_built,
            "{label}: seat {seat} is at {next} of {} frames",
            o.status.frames_built
        );
    }
    assert_eq!(o.status.verdicts, 0, "{label}: {:?}", o.status.recent_verdicts);
    assert!(
        o.status.lockstep.checkpoints_agreed * 30 + 120 >= o.status.frames_built,
        "{label}: {} checkpoints agreed over {} frames",
        o.status.lockstep.checkpoints_agreed,
        o.status.frames_built
    );
    let observer = o.status.observer.as_ref().expect("an observer");
    assert!(
        observer.attached && observer.voting && !observer.diverged,
        "{label}: {observer:?}"
    );
    // Nothing an honest seat sent was refused, and every input a frame lists is its seat's own.
    assert!(o.honest_refused.is_empty(), "{label}: {:?}", o.honest_refused);
    assert_eq!(o.seat_refused, 0, "{label}: host payloads a seat refused");
    assert!(
        o.foreign.is_empty(),
        "{label}: {:?}",
        &o.foreign[..o.foreign.len().min(8)]
    );
    assert!(o.delay_max <= o.max_delay, "{label}");
    // A silent seat is removed by the timeout policy: never before `timeout_ms` after the last
    // datagram it sent (the host counts from the last one it heard), within a second of
    // `timeout_ms` after it went silent, and frames go on.
    for (seat, at) in plan.silent.iter().chain(plan.hostile.iter()).copied() {
        let seat = seat as u16;
        let removal = o
            .removed
            .iter()
            .find(|r| r.0 == seat)
            .unwrap_or_else(|| panic!("{label}: silent seat {seat} was not removed: {:?}", o.removed));
        assert_eq!(removal.2, "timeout", "{label}: {removal:?}");
        let last_sent = o.last_sent.get(&seat).copied().unwrap_or(0);
        assert!(
            last_sent < at,
            "{label}: seat {seat} sent at {last_sent} ms, silent from {at} ms"
        );
        assert!(
            removal.3 >= last_sent + o.timeout_ms && removal.3 <= at + o.timeout_ms + 1_000,
            "{label}: seat {seat} silent at {at} ms (last sent {last_sent} ms), removed at {} ms (timeout {} ms)",
            removal.3,
            o.timeout_ms
        );
        let at_removal = o.frames_at_removal.get(&seat).copied().unwrap_or(0);
        assert!(
            o.status.frames_built >= at_removal + (plan.duration_ms - removal.3) * 60 / 1_000 - 2,
            "{label}: frames stopped after seat {seat} was removed"
        );
    }
    // Only the seats that went silent were removed.
    assert_eq!(
        o.removed.len(),
        usize::from(plan.silent.is_some()) + usize::from(plan.hostile.is_some()),
        "{label}: {:?}",
        o.removed
    );
}

/// Built with `--no-default-features`, regia_net has no ECS half: checked at compile time.
#[cfg(not(feature = "ecs"))]
const NO_ECS_IN_THIS_BUILD: () = assert!(
    !regia_net::ECS_FEATURE,
    "built with --no-default-features, regia_net has no ECS"
);

#[test]
fn forty_seats_on_bad_wifi_stay_in_step_and_a_silent_seat_is_removed() {
    #[cfg(not(feature = "ecs"))]
    let () = NO_ECS_IN_THIS_BUILD;
    let plan = Plan {
        profile: "bad-wifi",
        seats: 40,
        seed: 0xbadf1,
        duration_ms: 40_000,
        silent: Some((7, 10_000)),
        hostile: None,
    };
    let o = run(&plan);
    assert_healthy("bad-wifi", &plan, &o);
}

#[test]
fn forty_seats_on_mobile_4g_with_a_rebinding_stay_in_step() {
    let plan = Plan {
        profile: "mobile-4g",
        seats: 40,
        seed: 0x46,
        duration_ms: 40_000,
        silent: Some((13, 8_000)),
        hostile: None,
    };
    let o = run(&plan);
    assert_healthy("mobile-4g", &plan, &o);
}

#[test]
fn forty_seats_on_the_lockstep_mix_stay_in_step() {
    let plan = Plan {
        profile: "lockstep-mix",
        seats: 40,
        seed: 0x10c5,
        duration_ms: 40_000,
        silent: Some((21, 12_000)),
        hostile: None,
    };
    let o = run(&plan);
    assert_healthy("lockstep-mix", &plan, &o);
}

/// The `hostile` profile: good-wifi links and one hostile seat. It replays seat 0's datagrams as
/// its own (twice each), sends truncated copies, garbage datagrams and garbage stream payloads,
/// then goes silent. Seat 0's inputs it replayed land on the replaying seat, the only seat its
/// bytes were handed in for; no frame lists them under seat 0 twice or any input under a seat
/// that did not send it, and the honest seats' delay stays where their own links put it.
#[test]
fn a_hostile_seat_lands_nothing_on_another_seat_and_is_removed_when_silent() {
    let plan = Plan {
        profile: "hostile",
        seats: 40,
        seed: 0x6057,
        duration_ms: 40_000,
        silent: None,
        hostile: Some((39, 12_000)),
    };
    let o = run(&plan);
    assert_healthy("hostile", &plan, &o);
    assert!(o.replays_landed > 0, "the replayed inputs landed on the replayer");
    assert!(o.hostile_refused > 0, "the garbage was refused");
    assert!(
        o.delay_max <= o.initial_delay,
        "good-wifi seats and a replayer: delay {} over the initial {}",
        o.delay_max,
        o.initial_delay
    );
}
