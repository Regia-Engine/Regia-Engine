// SPDX-License-Identifier: MPL-2.0
//! The lockstep server game type (`LockstepServer`, ADR-8558) on the real endpoints (KI-NT-040,
//! ADR-8855): a `NetServer` and 40 `NetClient`s over the soak kit's impaired network, in virtual
//! time (a millisecond counter; no sleeps, no wall clock). Every lockstep payload goes through the
//! transport's handshake, encryption, congestion control, reliable channel, path validation and
//! keepalives, as in a game; nothing is modelled.
//!
//! `lockstep_server_soak.rs` keeps the lockstep layer's own seat-provenance checks on a modelled
//! stream (a hostile seat there hands the server another seat's plaintext payloads, which the
//! authenticated transport here never lets through). This file runs the same profiles end to end.
//!
//! Every test checks: every seat connects and stays connected; the seats that stayed step every
//! frame in order and agree on the state hash at their common tick, with no desync verdict and the
//! observer voting; frames are built on the clock from the start; nothing an honest seat sent is
//! refused by the server and nothing the server sent is refused by a seat; no seat but the quiet one
//! falls behind the frame log; every input a frame lists under a seat is one that seat submitted; a
//! seat whose game stops sending lockstep payloads (its connection stays up) is removed by the
//! timeout policy, never early, and frames go on.
//!
//! Run on the ECS-free core: `cargo test -p regia_net_soak --no-default-features --test
//! lockstep_server_endpoints` (the default build runs the same tests).

use std::collections::BTreeMap;
use std::net::SocketAddr;

use regia_net::lockstep::net::{LockstepCarrier, channel_lane, send_host, send_seat};
use regia_net::lockstep::server::{LockstepServer, LockstepServerSettings, LockstepServerStatus};
use regia_net::lockstep::{HostEvent, LockstepSeat, SeatId, SeatInput};
use regia_net::{
    ClientConfig, ClientEvent, DatagramSocket, NetClient, NetServer, PeerId, ServerConfig, ServerEvent,
};
use regia_net_soak::impair::{EndpointId, ImpairedNet, ImpairedSocket, SoakRng};
use regia_net_soak::lockstep::{InputMix, InputSource, LockGame, LockInput, LockSim};
use regia_net_soak::profiles::Profile;

/// A seat's application frame, virtual milliseconds.
const FRAME_MS: u64 = 16;
/// The first byte of an honest seat's input command: the next two are its seat id.
const HONEST_TAG: u8 = 0x5e;

fn server_addr() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 1], 7_778))
}

fn seat_addr(i: usize) -> SocketAddr {
    SocketAddr::from(([10, 2 + (i / 250) as u8, 0, (i % 250) as u8 + 1], 41_000))
}

fn attacker_addr() -> SocketAddr {
    SocketAddr::from(([10, 9, 9, 9], 6_666))
}

/// The command an honest seat tags every input with.
fn tag(seat: u16) -> Vec<u8> {
    vec![HONEST_TAG, seat as u8, (seat >> 8) as u8]
}

/// One seat: a real client, its lockstep seat and simulation.
struct Node {
    id: SeatId,
    ep: EndpointId,
    client: NetClient<LockGame, ImpairedSocket>,
    core: LockstepSeat<LockGame>,
    sim: LockSim,
    inputs: InputSource,
    hashes: BTreeMap<u64, u64>,
    phase: u64,
    connected: bool,
    ended: Option<String>,
    first_sent: bool,
    in_order: bool,
    /// From this virtual millisecond on the game sends no lockstep payload (the connection lives).
    silent_from: Option<u64>,
    last_sent_ms: u64,
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
    /// `(seat, ms)`: that seat's game stops sending at `ms`.
    silent: Option<(usize, u64)>,
    /// An off-path attacker sends garbage to the server, some of it with seat 0's address forged.
    attacker: bool,
}

/// What one run measured.
#[derive(Default)]
struct Outcome {
    status: LockstepServerStatus,
    started_ms: Option<u64>,
    timeout_ms: u64,
    initial_delay: u16,
    max_delay: u16,
    delay_max: u16,
    removed: Vec<(u16, u64, &'static str, u64)>,
    last_sent: BTreeMap<u16, u64>,
    frames_at_removal: BTreeMap<u16, u64>,
    foreign: Vec<String>,
    honest_refused: BTreeMap<String, u64>,
    transport_refused: u64,
    lagging: Vec<u16>,
    peers_left: Vec<String>,
    ended: Vec<(u16, String)>,
    not_connected: Vec<u16>,
    attacker_sent: u64,
    ticks: Vec<(u16, u64)>,
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
    let mut rng = SoakRng::derive(plan.seed, 0x4e50);
    let words = [rng.next_u64(), rng.next_u64(), rng.next_u64(), rng.next_u64()];
    let mut secret = [0u8; 32];
    for (i, b) in secret.iter_mut().enumerate() {
        *b = (words[i / 8] >> ((i % 8) * 8)) as u8;
    }
    let settings = LockstepServerSettings {
        enabled: true,
        ..LockstepServerSettings::default()
    };
    let mut server =
        LockstepServer::<LockGame>::new(&settings, 60, Some(Box::new(LockSim::default()))).expect("server");
    let config = server.config().clone();
    let mut transport = NetServer::<LockGame, _>::with_secret(
        net.socket(server_ep),
        ServerConfig {
            max_peers: plan.seats + 4,
            tick_rate_hz: config.tick_rate_hz,
            ..ServerConfig::default()
        },
        secret,
    );
    let mut nodes = Vec::with_capacity(plan.seats);
    for i in 0..plan.seats {
        let id = SeatId(i as u16);
        let ep = net
            .add_endpoint(seat_addr(i), profile.client_link(i, plan.duration_ms))
            .expect("seat endpoint");
        let client = NetClient::<LockGame, _>::connect(
            net.socket(ep),
            server_addr(),
            ClientConfig {
                name: format!("seat{i}"),
                ..ClientConfig::default()
            },
            &(i as u32),
        )
        .expect("connect");
        nodes.push(Node {
            id,
            ep,
            client,
            core: LockstepSeat::new(config.clone(), id),
            sim: LockSim::default(),
            inputs: InputSource::new(plan.seed, id.0, InputMix::ORDERS),
            hashes: BTreeMap::new(),
            phase: (i as u64 * 5) % FRAME_MS,
            connected: false,
            ended: None,
            first_sent: false,
            in_order: true,
            silent_from: plan.silent.and_then(|(s, at)| (s == i).then_some(at)),
            last_sent_ms: 0,
            refused: 0,
        });
    }
    let mut attacker = plan.attacker.then(|| {
        let ep = net
            .add_endpoint(attacker_addr(), profile.client_link(plan.seats, plan.duration_ms))
            .expect("attacker endpoint");
        (ep, net.socket(ep))
    });
    let mut out = Outcome {
        timeout_ms: config.timeout.timeout_ms,
        initial_delay: config.initial_delay_ticks,
        max_delay: config.max_delay_ticks,
        seats_in_order: true,
        ..Outcome::default()
    };
    let mut seat_of: BTreeMap<PeerId, SeatId> = BTreeMap::new();
    let mut peer_of: BTreeMap<SeatId, PeerId> = BTreeMap::new();
    let mut frames_built = 0u64;
    let mut now = 0u64;
    while now <= plan.duration_ms {
        net.advance_to_ms(now);
        for node in &mut nodes {
            if now % FRAME_MS != node.phase {
                continue;
            }
            node.client.receive(now);
            while let Some(event) = node.client.poll_event() {
                match event {
                    ClientEvent::Connected { .. } => node.connected = true,
                    ClientEvent::Ended(end) => {
                        node.connected = false;
                        node.ended = Some(end.as_str().to_owned());
                    }
                    ClientEvent::Message { channel, message } => {
                        let bytes = LockGame::unwrap(&message).unwrap_or_default();
                        if node.core.receive(channel_lane(channel), bytes, now).is_err() {
                            node.refused += 1;
                        }
                    }
                    _ => {}
                }
            }
            if node.connected {
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
                if !node.silent(now) {
                    let report = send_seat(&mut node.client, &mut node.core, now);
                    out.transport_refused += u64::from(report.datagrams_refused + report.stream_refused);
                    node.last_sent_ms = now;
                }
            }
            node.client.flush(now);
        }
        if let Some((ep, sock)) = attacker.as_mut()
            && now.is_multiple_of(FRAME_MS)
            && now >= 1_000
        {
            // Garbage from its own address and with seat 0's address forged, of every length up
            // to a datagram, at the client frame rate.
            let len = 1 + rng.up_to(1_200) as usize;
            let junk: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            let _ = sock.send_to(&junk, server_addr());
            net.send_forged(*ep, net.public_addr(nodes[0].ep), &junk, server_addr());
            out.attacker_sent += 2;
        }
        transport.receive(now);
        while let Some(event) = transport.poll_event() {
            match event {
                ServerEvent::PeerJoined { peer, join, .. } => {
                    let seat = SeatId(join as u16);
                    seat_of.insert(peer, seat);
                    peer_of.insert(seat, peer);
                    server.add_seat(seat).expect("a free seat");
                }
                ServerEvent::PeerLeft { peer, reason } => {
                    out.peers_left.push(format!("{peer:?}: {reason:?}"));
                }
                ServerEvent::Message {
                    peer,
                    channel,
                    message,
                } => {
                    let Some(seat) = seat_of.get(&peer).copied() else {
                        continue;
                    };
                    let Some(bytes) = LockGame::unwrap(&message) else {
                        continue;
                    };
                    if let Err(e) = server.receive(seat, channel_lane(channel), bytes, now)
                        && out.started_ms.is_some()
                    {
                        *out.honest_refused.entry(e.code().to_owned()).or_default() += 1;
                    }
                }
                _ => {}
            }
        }
        if now.is_multiple_of(1_000) {
            for row in transport.peer_rows() {
                if let Some(seat) = seat_of.get(&PeerId(row.id)) {
                    server.host_mut().note_transport_rtt(*seat, row.rtt_ms as u64);
                }
            }
        }
        if out.started_ms.is_none() && peer_of.len() == nodes.len() {
            server.start(now);
            out.started_ms = Some(now);
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
                if i.command != tag(seat.0) {
                    out.foreign.push(format!(
                        "tick {} seat {}: command {:?}",
                        frame.tick, seat.0, i.command
                    ));
                }
            }
        }
        while let Some(event) = server.poll_event() {
            match event {
                HostEvent::SeatRemoved { seat, tick, reason } => {
                    out.removed.push((seat.0, tick, reason, now));
                    out.frames_at_removal.insert(seat.0, frames_built);
                }
                HostEvent::SeatLagging { seat } => out.lagging.push(seat.0),
                _ => {}
            }
        }
        if !built.is_empty() {
            let report = send_host(
                &mut transport,
                server.host_mut(),
                |seat| peer_of.get(&seat).copied(),
                now,
            );
            out.transport_refused += u64::from(report.datagrams_refused + report.stream_refused);
        }
        transport.flush(now);
        now += 1;
    }
    out.status = server.status(now);
    out.last_sent = nodes.iter().map(|n| (n.id.0, n.last_sent_ms)).collect();
    out.ended = nodes
        .iter()
        .filter_map(|n| n.ended.clone().map(|e| (n.id.0, e)))
        .collect();
    out.not_connected = nodes.iter().filter(|n| !n.connected).map(|n| n.id.0).collect();
    let stayed: Vec<&Node> = nodes.iter().filter(|n| n.silent_from.is_none()).collect();
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
        "{label}: started {:?} frames {} delay_max {} agreed {} verdicts {} removed {:?} lagging {:?} \
         transport_refused {} honest_refused {:?} seat_refused {} peers_left {:?} attacker_sent {} common {:?}",
        o.started_ms,
        o.status.frames_built,
        o.delay_max,
        o.status.lockstep.checkpoints_agreed,
        o.status.verdicts,
        o.removed,
        o.lagging,
        o.transport_refused,
        o.honest_refused,
        o.seat_refused,
        o.peers_left,
        o.attacker_sent,
        o.common
    );
    // Every seat connected through the handshake and stayed connected.
    assert!(
        o.not_connected.is_empty(),
        "{label}: not connected: {:?}",
        o.not_connected
    );
    assert!(o.ended.is_empty(), "{label}: connections ended: {:?}", o.ended);
    assert!(o.peers_left.is_empty(), "{label}: peers left: {:?}", o.peers_left);
    // Frames are built on the clock from the start: one per tick, give or take the last.
    let started = o.started_ms.expect("every seat joined");
    let ticks_due = (plan.duration_ms - started) * 60 / 1_000;
    assert!(
        o.status.frames_built + 2 >= ticks_due,
        "{label}: {} frames in {} ms from the start at {started} ms",
        o.status.frames_built,
        plan.duration_ms - started
    );
    // The seats that stayed step every frame in order and agree.
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
    assert!(o.honest_refused.is_empty(), "{label}: {:?}", o.honest_refused);
    assert_eq!(o.seat_refused, 0, "{label}: host payloads a seat refused");
    // Only a seat that went quiet stops acknowledging frames and falls behind the frame log.
    let quiet = plan.silent.map(|(s, _)| s as u16);
    assert!(
        o.lagging.iter().all(|s| Some(*s) == quiet),
        "{label}: seats marked lagging: {:?}",
        o.lagging
    );
    assert!(
        o.foreign.is_empty(),
        "{label}: {:?}",
        &o.foreign[..o.foreign.len().min(8)]
    );
    assert!(o.delay_max <= o.max_delay, "{label}");
    if let Some((seat, at)) = plan.silent {
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
    assert_eq!(
        o.removed.len(),
        usize::from(plan.silent.is_some()),
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
fn forty_real_clients_on_bad_wifi_stay_in_step_and_a_silent_seat_is_removed() {
    #[cfg(not(feature = "ecs"))]
    let () = NO_ECS_IN_THIS_BUILD;
    let plan = Plan {
        profile: "bad-wifi",
        seats: 40,
        seed: 0xbadf1,
        duration_ms: 40_000,
        silent: Some((7, 10_000)),
        attacker: false,
    };
    let o = run(&plan);
    assert_healthy("bad-wifi", &plan, &o);
}

/// `mobile-4g` rebinds some seats' NAT mappings mid-match: the transport's path validation moves
/// the connection, and the lockstep seat never notices.
#[test]
fn forty_real_clients_on_mobile_4g_with_rebindings_stay_in_step() {
    let plan = Plan {
        profile: "mobile-4g",
        seats: 40,
        seed: 0x46,
        duration_ms: 40_000,
        silent: Some((13, 8_000)),
        attacker: false,
    };
    let o = run(&plan);
    assert_healthy("mobile-4g", &plan, &o);
}

#[test]
fn forty_real_clients_on_the_lockstep_mix_stay_in_step() {
    let plan = Plan {
        profile: "lockstep-mix",
        seats: 40,
        seed: 0x10c5,
        duration_ms: 40_000,
        silent: Some((21, 12_000)),
        attacker: false,
    };
    let o = run(&plan);
    assert_healthy("lockstep-mix", &plan, &o);
}

/// The `hostile` profile's links with an off-path attacker: garbage datagrams from its own address
/// and with seat 0's address forged, from the first second on. The transport drops all of it
/// before the lockstep layer: no seat is refused anything, seat 0 stays connected, and the delay
/// stays at the initial ticks.
#[test]
fn forty_real_clients_with_an_off_path_attacker_are_untouched() {
    let plan = Plan {
        profile: "hostile",
        seats: 40,
        seed: 0x6057,
        duration_ms: 40_000,
        silent: None,
        attacker: true,
    };
    let o = run(&plan);
    assert_healthy("hostile", &plan, &o);
    assert!(o.attacker_sent > 4_000, "{}", o.attacker_sent);
    assert!(
        o.delay_max <= o.initial_delay,
        "an attacker moved the delay to {} (initial {})",
        o.delay_max,
        o.initial_delay
    );
}
