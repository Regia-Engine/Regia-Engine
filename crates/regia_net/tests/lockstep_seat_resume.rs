// SPDX-License-Identifier: MPL-2.0
//! Wire 6 (spec 120 § 10.3, ADR-10052 (2)): a lockstep seat resumes with the transport's own resume
//! ticket, which rides the sealed handshake, so its first payload leaves right behind the accept
//! instead of after a token message (`LockstepHost::resume_token`) has crossed the link. Seeded
//! conditioner, virtual milliseconds, no wall clock.

use std::net::SocketAddr;

use regia_net::LoopbackSocket;
use regia_net::conditioner::ConditionerConfig;
use regia_net::lockstep::net::{
    LockstepCarrier, SeatLink, SeatPeers, host_receive, seat_receive, send_host, send_seat,
};
use regia_net::lockstep::{HostEvent, HostRole, LockstepConfig, LockstepHost, LockstepSeat, SeatId};
use regia_net::prelude::*;

const STEP_MS: u64 = 5;
/// One-way delay of each direction: a 120 ms round trip.
const DELAY_MS: u64 = 60;
const SEAT: SeatId = SeatId(1);

struct Game;
impl NetGame for Game {
    type Message = Vec<u8>;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.lockstep.seat_resume";
    const PROTOCOL_VERSION: u32 = 1;
}

/// Byte 0 marks a lockstep payload; byte 1 a seat's resume token (the token flow's own message).
impl LockstepCarrier for Game {
    fn wrap(payload: Vec<u8>) -> Vec<u8> {
        let mut m = Vec::with_capacity(payload.len() + 1);
        m.push(0);
        m.extend(payload);
        m
    }
    fn unwrap(message: &Vec<u8>) -> Option<&[u8]> {
        match message.split_first() {
            Some((0, rest)) => Some(rest),
            _ => None,
        }
    }
}

fn token_message(token: u64) -> Vec<u8> {
    let mut m = vec![1];
    m.extend(token.to_le_bytes());
    m
}

fn read_token(message: &[u8]) -> Option<u64> {
    match message.split_first() {
        Some((1, rest)) => Some(u64::from_le_bytes(rest.try_into().ok()?)),
        _ => None,
    }
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn delayed(seed: u64) -> ConditionerConfig {
    ConditionerConfig {
        enabled: true,
        seed,
        delay_ms: DELAY_MS,
        ..ConditionerConfig::default()
    }
}

fn client(
    net: &LoopbackNet,
    port: u16,
    resume: Option<[u8; regia_net::handshake::RESUME_TICKET_BYTES]>,
) -> NetClient<Game, LoopbackSocket> {
    let mut c = NetClient::connect(
        net.bind(addr(0)).expect("bind client"),
        addr(port),
        ClientConfig {
            name: "seat".into(),
            resume,
            ..ClientConfig::default()
        },
        &(),
    )
    .expect("join fits");
    c.set_conditioner(delayed(7));
    c
}

/// How the returning seat proves which seat it held.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Flow {
    /// The transport's resume ticket in the handshake; `SeatPeers` resumes the seat on
    /// `PeerResumed` (ADR-10052 (2)).
    Ticket,
    /// A fresh join, then the seat sends `LockstepHost::resume_token` as a game message.
    Token,
}

struct Match {
    net: LoopbackNet,
    port: u16,
    server: NetServer<Game, LoopbackSocket>,
    host: LockstepHost<Game>,
    seats: SeatPeers,
    now: u64,
    links: Vec<SeatLink>,
    /// The newest frame the host built.
    newest: Option<u64>,
    /// Seats the host marked lagging (their stream payloads had no connection) and has not sent
    /// a catch-up yet.
    lagging: std::collections::BTreeSet<SeatId>,
}

impl Match {
    fn new(port: u16) -> Self {
        let net = LoopbackNet::new();
        let mut server = NetServer::<Game, _>::with_secret(
            net.bind(addr(port)).expect("bind server"),
            ServerConfig {
                resume_grace_ms: 30_000,
                ..ServerConfig::default()
            },
            [23; 32],
        );
        server.set_conditioner(delayed(3));
        let mut host = LockstepHost::<Game>::new(LockstepConfig::default(), HostRole::Observer);
        host.add_seat(SEAT);
        Self {
            net,
            port,
            server,
            host,
            seats: SeatPeers::default(),
            now: 0,
            links: Vec::new(),
            newest: None,
            lagging: std::collections::BTreeSet::new(),
        }
    }

    /// One host pass: events (a join takes the seat when nobody holds it), payloads, frames out.
    fn host_pass(&mut self) {
        self.server.receive(self.now);
        while let Some(event) = self.server.poll_event() {
            if let Some(link) = self.seats.on_event(&mut self.host, &event, self.now) {
                self.links.push(link);
            }
            match event {
                ServerEvent::PeerJoined { peer, .. }
                    if self.seats.peer_of(SEAT).is_none()
                        && self.seats.seat_of(peer).is_none()
                        && !self.host.started() =>
                {
                    self.seats.bind(SEAT, peer);
                }
                ServerEvent::Message {
                    peer,
                    channel,
                    message,
                } => {
                    if let Some(token) = read_token(&message) {
                        // The token flow: the seat names itself after its fresh join.
                        if let Ok(seat) = self.host.resume(token, self.now) {
                            self.seats.bind(seat, peer);
                        }
                    } else if let Some(seat) = self.seats.seat_of(peer) {
                        let _ = host_receive(&mut self.host, seat, channel, &message, self.now);
                    }
                }
                _ => {}
            }
        }
        if !self.host.started() && self.seats.peer_of(SEAT).is_some() {
            self.host.start(self.now);
        }
        while let Some(frame) = self.host.poll_frame(self.now) {
            self.newest = Some(frame.tick);
        }
        while let Some(event) = self.host.poll_event() {
            if let HostEvent::SeatLagging { seat } = event {
                self.lagging.insert(seat);
            }
        }
        // A lagging seat with a connection gets the game's state (here: none) and the frames
        // since, as soon as it has one: for the ticket flow that is the pass of its resume.
        let ready: Vec<SeatId> = self
            .lagging
            .iter()
            .copied()
            .filter(|seat| self.seats.peer_of(*seat).is_some())
            .collect();
        for seat in ready {
            if let Some(tick) = self.newest
                && self.host.catch_up(seat, tick, &[], self.now).is_ok()
            {
                self.lagging.remove(&seat);
            }
        }
        let seats = &self.seats;
        send_host(&mut self.server, &mut self.host, |s| seats.peer_of(s), self.now);
        self.server.flush(self.now);
    }
}

/// One seat pass: events, payloads in, inputs out. Returns whether a lockstep payload arrived
/// (accepted or not: a seat that missed the outage's frames may refuse the first ones and ask for
/// a repair; the payload still came).
fn seat_pass(c: &mut NetClient<Game, LoopbackSocket>, seat: &mut LockstepSeat<Game>, now: u64) -> bool {
    c.receive(now);
    let mut got = false;
    while let Some(event) = c.poll_event() {
        if let ClientEvent::Message { channel, message } = event
            && seat_receive(seat, channel, &message, now).is_some()
        {
            got = true;
        }
    }
    while seat.next_frame().is_some() {
        let _ = seat.submit_input(&0);
    }
    send_seat(c, seat, now);
    c.flush(now);
    got
}

/// Plays a match, cuts the seat off until its peer is dormant, brings it back with `flow`, and
/// returns the milliseconds from the new connect to the seat's first lockstep payload, with the
/// match's link events.
fn outage_and_return(flow: Flow, port: u16) -> (u64, Vec<SeatLink>, Option<PeerId>, Option<PeerId>) {
    let mut m = Match::new(port);
    let mut c = client(&m.net, m.port, None);
    let mut seat = LockstepSeat::<Game>::new(LockstepConfig::default(), SEAT);
    let _ = seat.submit_input(&0);
    // Two seconds of play.
    while m.now < 2_000 {
        m.now += STEP_MS;
        seat_pass(&mut c, &mut seat, m.now);
        m.host_pass();
    }
    assert!(c.is_connected(), "the seat joined, state {:?}", c.state());
    assert!(m.host.started(), "the match started");
    let first_peer = m.seats.peer_of(SEAT);
    let ticket = c.resume_ticket();
    let token = m.host.resume_token(SEAT).expect("a seat token");
    // The seat's link dies: nothing from it until the transport holds its peer dormant.
    drop(c);
    for _ in 0..20_000 {
        m.now += STEP_MS;
        m.host_pass();
        if m.server.dormant_peers().count() == 1 {
            break;
        }
    }
    assert_eq!(
        m.server.dormant_peers().count(),
        1,
        "the silent seat's peer is dormant"
    );
    // It comes back from a new socket.
    let back_at = m.now;
    let mut c = match flow {
        Flow::Ticket => client(&m.net, m.port, Some(ticket.expect("the accept carried a ticket"))),
        Flow::Token => client(&m.net, m.port, None),
    };
    let mut token_sent = false;
    let mut arrived = None;
    while m.now < back_at + 5_000 {
        m.now += STEP_MS;
        if seat_pass(&mut c, &mut seat, m.now) && arrived.is_none() {
            arrived = Some(m.now - back_at);
            break;
        }
        if flow == Flow::Token && c.is_connected() && !token_sent {
            token_sent = c.send(Channel::GAME_RPC, &token_message(token)).is_ok();
            c.flush(m.now);
        }
        m.host_pass();
    }
    let Some(at) = arrived else {
        panic!(
            "{flow:?}: the seat never heard from the host again: connected {}, links {:?}, lagging {:?}, members {:?}",
            c.is_connected(),
            m.links,
            m.lagging,
            m.host.members().collect::<Vec<_>>()
        );
    };
    (at, m.links, first_peer, m.seats.peer_of(SEAT))
}

/// The ticket flow: the seat's peer goes dormant and comes back as the same `PeerId`, the seat
/// resumes in the pass of its `PeerResumed`, and its first payload arrives at least one round trip
/// before the token flow's (which joins as a new peer and has to send its token first).
#[test]
fn a_lockstep_seat_resumes_with_the_transport_ticket_one_round_trip_sooner() {
    let (ticket_ms, links, before, after) = outage_and_return(Flow::Ticket, 7841);
    assert!(links.contains(&SeatLink::Dormant { seat: SEAT }), "{links:?}");
    assert!(links.contains(&SeatLink::Resumed { seat: SEAT }), "{links:?}");
    assert_eq!(before, after, "the same PeerId holds the seat after the resume");
    let (token_ms, _, before_t, after_t) = outage_and_return(Flow::Token, 7842);
    assert_ne!(before_t, after_t, "the token flow's fresh join is a new PeerId");
    eprintln!(
        "seat resume, first payload after the new connect: ticket {ticket_ms} ms, token {token_ms} ms (round trip {} ms)",
        2 * DELAY_MS
    );
    // The token flow pays one more round trip (token up, frames down). The host sends frames on
    // its 60 Hz frame clock, so either flow's first frame may wait up to a frame period (17 ms)
    // for the next build: two of them are the alignment allowance, not a tolerance on the claim.
    assert!(
        ticket_ms + 2 * DELAY_MS <= token_ms + 2 * 17,
        "the ticket flow saves the token's round trip: ticket {ticket_ms} ms, token {token_ms} ms"
    );
}

/// A seat whose resume window passed is refused on its peer's return and unbound.
#[test]
fn a_seat_past_its_resume_window_is_refused_on_the_peer_resume() {
    let mut host = LockstepHost::<Game>::new(LockstepConfig::default(), HostRole::Observer);
    host.add_seat(SEAT);
    host.start(0);
    let mut seats = SeatPeers::default();
    seats.bind(SEAT, PeerId(3));
    host.remove_seat(SEAT);
    while host.poll_frame(1_000).is_some() {}
    assert_eq!(
        host.resume_seat(SEAT, 1_000),
        Err(regia_net::lockstep::LockstepError::ResumeRefused),
        "a seat that left is not resumable"
    );
    assert_eq!(
        host.resume_seat(SeatId(9), 1_000),
        Err(regia_net::lockstep::LockstepError::ResumeRefused),
        "nor an unknown one"
    );
}

/// The binding table on its own: dormant peers send nothing, a rebind moves the seat.
#[test]
fn seat_peers_follow_binds_and_dormancy() {
    let mut seats = SeatPeers::default();
    seats.bind(SeatId(1), PeerId(4));
    seats.bind(SeatId(2), PeerId(5));
    assert_eq!(seats.peer_of(SeatId(1)), Some(PeerId(4)));
    seats.bind(SeatId(1), PeerId(6));
    assert_eq!(seats.seat_of(PeerId(4)), None, "the old peer lost the seat");
    assert_eq!(seats.seat_of(PeerId(6)), Some(SeatId(1)));
    seats.unbind(SeatId(2));
    assert_eq!(seats.peer_of(SeatId(2)), None);
}
