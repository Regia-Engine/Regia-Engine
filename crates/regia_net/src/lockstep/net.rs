// SPDX-License-Identifier: MPL-2.0
//! Lockstep over `NetServer` / `NetClient` game messages (no framing change).
//!
//! The core speaks two lanes ([`Lane`]); the endpoints carry game messages on two channels. A game
//! names how its message type wraps a lockstep payload ([`LockstepCarrier`]) and these helpers move
//! the payloads: [`Lane::Datagram`] on [`Channel::Unreliable`], [`Lane::Stream`] on
//! [`Channel::GAME_RPC`]. The datagrams are the match's inputs (frames down, a seat's inputs up),
//! so they go in the transport's essential class: the rate controller never paces them below its
//! essential floor (spec 120 § 4.2 item 4, ADR-8487). One exception keeps the lanes in order: a
//! host frames datagram that continues frames sent on the stream to the same seat in the same
//! pass goes in the ordinary class, behind the stream, since the essential class leaves first and
//! a seat that sees frames past its chain reports a hole (ADR-8534).
//!
//! ```
//! use regia_net::lockstep::net::LockstepCarrier;
//! use regia_net::NetGame;
//!
//! struct Game;
//! impl NetGame for Game {
//!     type Message = Vec<u8>;
//!     type Input = u8;
//!     type JoinData = ();
//!     const GAME_ID: &'static str = "doc.lockstep.net";
//!     const PROTOCOL_VERSION: u32 = 1;
//! }
//! // A game whose only messages are lockstep payloads: the message is the payload.
//! impl LockstepCarrier for Game {
//!     fn wrap(payload: Vec<u8>) -> Vec<u8> {
//!         payload
//!     }
//!     fn unwrap(message: &Vec<u8>) -> Option<&[u8]> {
//!         Some(message)
//!     }
//! }
//! assert_eq!(Game::unwrap(&Game::wrap(vec![1, 2])), Some(&[1u8, 2][..]));
//! ```

use std::collections::BTreeMap;

use super::{Lane, LockstepError, LockstepHost, LockstepSeat, Outgoing, SeatId};
use crate::client::NetClient;
use crate::connection::{Channel, SendError};
use crate::payload::NetGame;
use crate::server::{NetServer, ServerEvent};
use crate::session::PeerId;
use crate::socket::DatagramSocket;
use crate::status::NetAgentState;

/// How a game's message type carries lockstep payloads.
pub trait LockstepCarrier: NetGame {
    /// Wraps a lockstep payload in a game message.
    fn wrap(payload: Vec<u8>) -> Self::Message;
    /// The lockstep payload inside a game message; `None` for the game's other messages.
    fn unwrap(message: &Self::Message) -> Option<&[u8]>;
}

/// The channel a lane travels on.
pub fn lane_channel(lane: Lane) -> Channel {
    match lane {
        Lane::Datagram => Channel::Unreliable,
        Lane::Stream => Channel::GAME_RPC,
    }
}

/// The lane a channel carries.
pub fn channel_lane(channel: Channel) -> Lane {
    match channel {
        Channel::Unreliable => Lane::Datagram,
        Channel::Stream(_) => Lane::Stream,
    }
}

/// What a send pass could not hand to the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SendReport {
    /// Datagram payloads the transport refused (the next datagram repeats their frames).
    pub datagrams_refused: u32,
    /// Stream payloads the transport refused, or that were meant for a seat without a
    /// connection. On the host the seat is marked lagging: what was meant for the stream is
    /// gone and it needs a state transfer.
    pub stream_refused: u32,
}

/// Sends everything `host` has for its seats through `server`. `peer_of` names each seat's
/// connection. A seat without one gets nothing: its datagrams are dropped (the next one repeats
/// their frames), and a stream payload marks it lagging like a refused one.
pub fn send_host<G: LockstepCarrier, S: DatagramSocket>(
    server: &mut NetServer<G, S>,
    host: &mut LockstepHost<G>,
    peer_of: impl Fn(SeatId) -> Option<PeerId>,
    now_ms: u64,
) -> SendReport {
    let mut report = SendReport::default();
    let mut streamed = StreamedSeats::default();
    for (seat, out) in host.outgoing(now_ms) {
        let lane = out.lane;
        let Some(peer) = peer_of(seat) else {
            if lane == Lane::Stream {
                report.stream_refused += 1;
                host.mark_lagging(seat);
            }
            continue;
        };
        let behind_stream = streamed.note(seat, lane);
        if host_send(server, peer, lane, behind_stream, &G::wrap(out.bytes)).is_err() {
            match lane {
                Lane::Datagram => report.datagrams_refused += 1,
                Lane::Stream => {
                    report.stream_refused += 1;
                    host.mark_lagging(seat);
                }
            }
        }
    }
    report
}

/// Sends the payloads of a [`HostEvent::SeatHandover`](super::HostEvent::SeatHandover) to `peer`,
/// the connection that held the seat before it was added again: the frames that holder still
/// lacks, through the one that lists its leave. A refused payload is counted; the holder is
/// leaving, so nothing is marked lagging for it.
pub fn send_handover<G: LockstepCarrier, S: DatagramSocket>(
    server: &mut NetServer<G, S>,
    peer: PeerId,
    payloads: Vec<Outgoing>,
) -> SendReport {
    let mut report = SendReport::default();
    let mut streamed = false;
    for out in payloads {
        let lane = out.lane;
        streamed |= lane == Lane::Stream;
        if host_send(server, peer, lane, streamed, &G::wrap(out.bytes)).is_err() {
            match lane {
                Lane::Datagram => report.datagrams_refused += 1,
                Lane::Stream => report.stream_refused += 1,
            }
        }
    }
    report
}

/// The seats that were handed a stream payload in one send pass (ADR-8534).
#[derive(Default)]
struct StreamedSeats(Vec<SeatId>);

impl StreamedSeats {
    /// Notes a payload for `seat` on `lane`; true when it is a datagram that follows a stream
    /// payload for the same seat in this pass.
    fn note(&mut self, seat: SeatId, lane: Lane) -> bool {
        let streamed = self.0.contains(&seat);
        match lane {
            Lane::Stream if !streamed => {
                self.0.push(seat);
                false
            }
            Lane::Stream => false,
            Lane::Datagram => streamed,
        }
    }
}

/// One host payload on its lane: a datagram in the essential class (the frames are the match's
/// inputs, ADR-8487), a stream payload on the reliable channel. A datagram `behind_stream` (it
/// continues frames the same pass sent on the stream) goes in the ordinary class instead: the
/// transport sends the essential class ahead of the reliable stream, and a seat that got these
/// frames before the stream's would see them past its chain, drop them and report a hole, whose
/// answer is more stream (ADR-8534).
fn host_send<G: LockstepCarrier, S: DatagramSocket>(
    server: &mut NetServer<G, S>,
    peer: PeerId,
    lane: Lane,
    behind_stream: bool,
    message: &G::Message,
) -> Result<(), SendError> {
    match lane {
        Lane::Datagram if behind_stream => server.send(peer, Channel::Unreliable, message),
        Lane::Datagram => server.send_essential_message(peer, message),
        Lane::Stream => server.send(peer, Channel::GAME_RPC, message),
    }
}

/// Sends everything `seat` has for the host through `client`.
pub fn send_seat<G: LockstepCarrier, S: DatagramSocket>(
    client: &mut NetClient<G, S>,
    seat: &mut LockstepSeat<G>,
    now_ms: u64,
) -> SendReport {
    let mut report = SendReport::default();
    for out in seat.outgoing(now_ms) {
        let lane = out.lane;
        let message = G::wrap(out.bytes);
        let sent = match lane {
            // The seat's inputs, in the essential class (ADR-8487).
            Lane::Datagram => client.send_essential_message(&message),
            Lane::Stream => client.send(Channel::GAME_RPC, &message),
        };
        if sent.is_err() {
            match lane {
                Lane::Datagram => report.datagrams_refused += 1,
                Lane::Stream => report.stream_refused += 1,
            }
        }
    }
    report
}

/// Publishes `host`'s status under `label` for the agent method `net.lockstep`.
pub fn publish_host<G: NetGame>(
    host: &mut LockstepHost<G>,
    label: &str,
    now_ms: u64,
    state: &mut NetAgentState,
) {
    state.publish_lockstep(label, host.status(now_ms));
}

/// Publishes `seat`'s status under `label` for the agent method `net.lockstep`.
pub fn publish_seat<G: NetGame>(seat: &LockstepSeat<G>, label: &str, state: &mut NetAgentState) {
    state.publish_lockstep(label, seat.status());
}

/// Hands a game message a seat's connection delivered to `host`. `None` when the message is not a
/// lockstep payload (the game handles it).
///
/// A refusal came over the seat's own authenticated connection: give it to a [`SeatScores`] and
/// remove the seat (and kick its peer) when that returns true (ADR-8734), as
/// `server_host::LockstepHostGame` does.
pub fn host_receive<G: LockstepCarrier>(
    host: &mut LockstepHost<G>,
    seat: SeatId,
    channel: Channel,
    message: &G::Message,
    now_ms: u64,
) -> Option<Result<(), LockstepError>> {
    let bytes = G::unwrap(message)?;
    Some(host.receive(seat, channel_lane(channel), bytes, now_ms))
}

/// Hands a game message the host's connection delivered to `seat`. `None` when the message is not
/// a lockstep payload.
pub fn seat_receive<G: LockstepCarrier>(
    seat: &mut LockstepSeat<G>,
    channel: Channel,
    message: &G::Message,
    now_ms: u64,
) -> Option<Result<(), LockstepError>> {
    let bytes = G::unwrap(message)?;
    Some(seat.receive(channel_lane(channel), bytes, now_ms))
}

/// What [`SeatPeers::on_event`] did with a server event for a seat's connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatLink {
    /// The seat's peer went dormant (the transport keeps it through `resume_grace_ms`): the seat
    /// keeps its binding, and sends to it are dropped until it comes back.
    Dormant {
        /// The seat.
        seat: SeatId,
    },
    /// The peer came back with its resume ticket and the host resumed its seat
    /// ([`LockstepHost::resume_seat`]) in the same pass: frames to it go out from now on. Call
    /// [`LockstepHost::catch_up`] when the seat was removed meanwhile (it is not among
    /// [`LockstepHost::members`]) or lost its state.
    Resumed {
        /// The seat.
        seat: SeatId,
    },
    /// The peer came back but the host no longer lets the seat return (it left, or its resume
    /// window passed): the binding is gone. Kick the peer or seat it anew.
    Refused {
        /// The seat.
        seat: SeatId,
    },
    /// The peer left for good (a goodbye, a kick, or the end of its grace): the binding is gone.
    Lost {
        /// The seat.
        seat: SeatId,
    },
}

/// Which transport peer holds which seat, kept from `NetServer`'s events (wire 6, spec 120 § 10.3,
/// ADR-10052 (2)). The seat's credential is the transport's own resume ticket, which the client
/// presents inside the sealed handshake: when its peer comes back as `ServerEvent::PeerResumed`
/// the seat resumes in the same server pass, so its first frames leave right behind the accept,
/// one round trip after the client's resume flight, with no token message first
/// ([`LockstepHost::resume_token`] stays for a seat process that restarted and holds no ticket).
/// Needs `ServerConfig::resume_grace_ms` above 0 (without it a lost peer leaves at once).
///
/// ```
/// use regia_net::lockstep::SeatId;
/// use regia_net::lockstep::net::SeatPeers;
/// use regia_net::session::PeerId;
///
/// let mut seats = SeatPeers::default();
/// seats.bind(SeatId(1), PeerId(4));
/// assert_eq!(seats.peer_of(SeatId(1)), Some(PeerId(4)));
/// assert_eq!(seats.seat_of(PeerId(4)), Some(SeatId(1)));
/// ```
#[derive(Debug, Clone, Default)]
pub struct SeatPeers {
    peers: BTreeMap<SeatId, PeerId>,
    seats: BTreeMap<PeerId, SeatId>,
    dormant: std::collections::BTreeSet<PeerId>,
}

impl SeatPeers {
    /// Binds `seat` to `peer` (a fresh join the game seated), replacing either side's old binding.
    pub fn bind(&mut self, seat: SeatId, peer: PeerId) {
        if let Some(old) = self.peers.insert(seat, peer) {
            self.seats.remove(&old);
            self.dormant.remove(&old);
        }
        if let Some(old) = self.seats.insert(peer, seat)
            && old != seat
        {
            self.peers.remove(&old);
        }
        self.dormant.remove(&peer);
    }

    /// Forgets `seat`'s binding.
    pub fn unbind(&mut self, seat: SeatId) {
        if let Some(peer) = self.peers.remove(&seat) {
            self.seats.remove(&peer);
            self.dormant.remove(&peer);
        }
    }

    /// The live connection of `seat`: `None` while its peer is dormant. Pass it to [`send_host`].
    pub fn peer_of(&self, seat: SeatId) -> Option<PeerId> {
        self.peers
            .get(&seat)
            .copied()
            .filter(|peer| !self.dormant.contains(peer))
    }

    /// The seat `peer` holds, dormant or not.
    pub fn seat_of(&self, peer: PeerId) -> Option<SeatId> {
        self.seats.get(&peer).copied()
    }

    /// Follows one server event for a bound peer; `None` for events of other peers and kinds.
    pub fn on_event<G: NetGame>(
        &mut self,
        host: &mut LockstepHost<G>,
        event: &ServerEvent<G>,
        now_ms: u64,
    ) -> Option<SeatLink> {
        match event {
            ServerEvent::PeerDormant { peer, .. } => {
                let seat = self.seat_of(*peer)?;
                self.dormant.insert(*peer);
                Some(SeatLink::Dormant { seat })
            }
            ServerEvent::PeerResumed { peer, .. } => {
                let seat = self.seat_of(*peer)?;
                self.dormant.remove(peer);
                match host.resume_seat(seat, now_ms) {
                    Ok(_) => Some(SeatLink::Resumed { seat }),
                    Err(_) => {
                        self.unbind(seat);
                        Some(SeatLink::Refused { seat })
                    }
                }
            }
            ServerEvent::PeerLeft { peer, .. } => {
                let seat = self.seat_of(*peer)?;
                self.unbind(seat);
                Some(SeatLink::Lost { seat })
            }
            _ => None,
        }
    }
}

/// Malformed points one refused lockstep payload scores (ADR-8734): what a transport
/// malformation scores ([`crate::server::MALFORMED_WEIGHT`]).
pub const SEAT_MALFORMED_WEIGHT: u32 = crate::server::MALFORMED_WEIGHT;

/// Per-seat malformed scores of a lockstep host (ADR-8734), the transport's rule
/// (ADR-8181/8606) for payloads the seat's own connection delivered: each refusal that
/// [`LockstepError::is_malformation`] adds [`SEAT_MALFORMED_WEIGHT`], the score leaks
/// [`crate::inbound::MALFORMED_LEAK_PER_S`] points a second (exactly, on the host's millisecond
/// clock), and at [`crate::server::MALFORMED_SCORE_LIMIT`] the seat is out. One refusal is never
/// fatal; a seat that keeps sending them is removed in about a second.
#[derive(Debug, Clone, Default)]
pub struct SeatScores {
    seats: BTreeMap<SeatId, SeatScore>,
}

#[derive(Debug, Clone, Copy)]
struct SeatScore {
    /// Points times 1,000.
    milli: u64,
    /// The millisecond the leak was taken up to.
    clock_ms: u64,
}

impl SeatScores {
    /// Thousandths of a point per point.
    const SCALE: u64 = 1_000;

    /// Scores `error` from `seat` at `now_ms`; true when the seat's score reached the limit (the
    /// caller removes the seat and drops its connection). An error that is not a malformation
    /// scores nothing and returns false.
    pub fn charge(&mut self, seat: SeatId, error: &LockstepError, now_ms: u64) -> bool {
        if !error.is_malformation() {
            return false;
        }
        let score = self.seats.entry(seat).or_insert(SeatScore {
            milli: 0,
            clock_ms: now_ms,
        });
        // The leak is MALFORMED_LEAK_PER_S points a second = that many thousandths a millisecond.
        let elapsed = now_ms.saturating_sub(score.clock_ms);
        score.clock_ms = score.clock_ms.max(now_ms);
        score.milli = score
            .milli
            .saturating_sub(crate::inbound::MALFORMED_LEAK_PER_S.saturating_mul(elapsed))
            .saturating_add(u64::from(SEAT_MALFORMED_WEIGHT).saturating_mul(Self::SCALE));
        score.milli >= u64::from(crate::server::MALFORMED_SCORE_LIMIT).saturating_mul(Self::SCALE)
    }

    /// `seat`'s score in whole points at its last charge (0 for a seat never charged).
    pub fn points(&self, seat: SeatId) -> u64 {
        self.seats.get(&seat).map_or(0, |s| s.milli / Self::SCALE)
    }

    /// Forgets `seat` (it left, or was removed).
    pub fn remove(&mut self, seat: SeatId) {
        self.seats.remove(&seat);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-8734: 64 malformed refusals at one instant reach the 1,024-point limit; 63 do not; a
    /// state refusal (unknown seat) scores nothing; the score leaks 4 points a second.
    #[test]
    fn seat_scores_reach_the_limit_at_64_malformations_and_leak() {
        let mut s = SeatScores::default();
        let bad = LockstepError::Malformed("kind");
        for n in 1..64 {
            assert!(!s.charge(SeatId(1), &bad, 1_000), "refusal {n}");
        }
        assert_eq!(s.points(SeatId(1)), 63 * 16);
        assert!(!s.charge(SeatId(1), &LockstepError::UnknownSeat(SeatId(1)), 1_000));
        assert_eq!(s.points(SeatId(1)), 63 * 16, "a state refusal scores nothing");
        // Four seconds leak 16 points: the 64th refusal then leaves it just under the limit.
        assert!(!s.charge(SeatId(1), &bad, 5_000));
        assert_eq!(s.points(SeatId(1)), 64 * 16 - 16);
        assert!(
            s.charge(SeatId(1), &bad, 5_000),
            "the next one at the same instant is out"
        );
        assert_eq!(s.points(SeatId(2)), 0, "per seat");
        s.remove(SeatId(1));
        assert_eq!(s.points(SeatId(1)), 0);
    }

    /// ADR-8534: a seat's datagram that follows a stream payload for the same seat in the same
    /// pass is behind the stream; other seats' datagrams, and a datagram before any stream
    /// payload, are not.
    #[test]
    fn a_datagram_after_the_seats_stream_payload_is_behind_the_stream() {
        let mut s = StreamedSeats::default();
        assert!(!s.note(SeatId(1), Lane::Datagram), "no stream yet");
        assert!(!s.note(SeatId(1), Lane::Stream));
        assert!(!s.note(SeatId(1), Lane::Stream), "a second stream payload");
        assert!(s.note(SeatId(1), Lane::Datagram), "behind seat 1's stream");
        assert!(!s.note(SeatId(2), Lane::Datagram), "seat 2 had no stream");
    }
}
