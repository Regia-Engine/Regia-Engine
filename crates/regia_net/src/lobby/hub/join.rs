// SPDX-License-Identifier: MPL-2.0
//! Join brokering (NL-05, ADR-8607): how a member reaches the lobby owner's game session.
//!
//! The hub never carries game datagrams. The owner offers its game socket's reach (a
//! [`ReachOffer`]: candidates, the relay and its join code, the join ticket with the pinned server
//! key); a member asks for a path ([`HubRequest::AskReach`](super::HubRequest::AskReach)) with its
//! own candidates; the owner approves; the hub then sends each side the other's candidate set and
//! a [`JoinPlan`] whose path is chosen in a fixed order:
//!
//! 1. **mapped**: the owner announced a port mapping (a [`CandidateKind::Mapped`] candidate): dial
//!    it directly, nothing else is tried;
//! 2. **punch**: paced connectivity checks from the game sockets (`crate::nat`, ICE-shaped, through
//!    the relay's introduction), with the relay as the fallback at 1.5 s;
//! 3. **relay**: when both sides told the hub their NAT behavior and the RFC 4787 model says no
//!    direct pair can work ([`crate::relay::simulate_hole_punch`]), the relay allocation (N113) at
//!    once, without the punch wait.
//!
//! The member reports the path that carried its sealed handshake
//! ([`HubRequest::ReachDone`](super::HubRequest::ReachDone)); the hub records it per member for
//! `lobby.hub_join_status` and tells the owner.
//!
//! The order, the mapped-first rule and the punch-or-relay prediction follow DO27's `ir-hub`
//! `join.rs` as requirements only (`docs/do27-port-review.md` § 2); no line is copied.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::crypto::{PlayerKey, ServerKey};
use crate::lobby::{LobbyId, MemberId};
use crate::nat::{Candidate, CandidateKind};
use crate::relay::{NatFiltering, NatMapping, PunchResult, SimNat, simulate_hole_punch};

/// Longest join ticket an offer carries, bytes.
pub const MAX_TICKET_BYTES: usize = 128;

/// Which path a join takes, in the broker's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ReachKind {
    /// The owner's port mapping, dialed directly.
    Mapped,
    /// A direct pair found by connectivity checks.
    Punch,
    /// The relay allocation.
    Relay,
}

impl ReachKind {
    /// Stable lowercase name (`mapped`, `punch`, `relay`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mapped => "mapped",
            Self::Punch => "punch",
            Self::Relay => "relay",
        }
    }

    /// The wire value.
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Mapped => 0,
            Self::Punch => 1,
            Self::Relay => 2,
        }
    }

    /// Parses a wire value.
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Mapped,
            1 => Self::Punch,
            2 => Self::Relay,
            _ => return None,
        })
    }
}

/// A side's NAT behavior as it told the hub (RFC 4787 mapping and filtering). Unknown is `None` on
/// the wire and in [`ReachOffer::nat`]: the broker then plans a punch with the relay fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NatClass {
    /// Mapping behavior (RFC 4787 § 4.1).
    pub mapping: NatMapping,
    /// Filtering behavior (RFC 4787 § 5).
    pub filtering: NatFiltering,
}

impl NatClass {
    /// Endpoint-independent mapping and filtering ("full cone").
    pub const FULL_CONE: Self = Self {
        mapping: NatMapping::EndpointIndependent,
        filtering: NatFiltering::EndpointIndependent,
    };
    /// Endpoint-independent mapping, address- and port-dependent filtering ("port restricted").
    pub const PORT_RESTRICTED: Self = Self {
        mapping: NatMapping::EndpointIndependent,
        filtering: NatFiltering::AddressAndPortDependent,
    };
    /// Address- and port-dependent mapping and filtering ("symmetric").
    pub const SYMMETRIC: Self = Self {
        mapping: NatMapping::AddressAndPortDependent,
        filtering: NatFiltering::AddressAndPortDependent,
    };

    /// The wire byte: mapping in the high nibble, filtering in the low one (each 0..=2).
    pub fn to_u8(self) -> u8 {
        let m = match self.mapping {
            NatMapping::EndpointIndependent => 0,
            NatMapping::AddressDependent => 1,
            NatMapping::AddressAndPortDependent => 2,
        };
        let f = match self.filtering {
            NatFiltering::EndpointIndependent => 0,
            NatFiltering::AddressDependent => 1,
            NatFiltering::AddressAndPortDependent => 2,
        };
        (m << 4) | f
    }

    /// Parses the wire byte.
    pub fn from_u8(v: u8) -> Option<Self> {
        let mapping = *NatMapping::ALL.get(usize::from(v >> 4))?;
        let filtering = *NatFiltering::ALL.get(usize::from(v & 0x0F))?;
        Some(Self { mapping, filtering })
    }

    /// Stable lowercase name (`full_cone`, `port_restricted`, `symmetric`, or
    /// `<mapping>_<filtering>` with `eim`/`adm`/`apdm` and `eif`/`adf`/`apdf`).
    pub fn name(self) -> String {
        match self {
            Self::FULL_CONE => "full_cone".into(),
            Self::PORT_RESTRICTED => "port_restricted".into(),
            Self::SYMMETRIC => "symmetric".into(),
            other => {
                let m = ["eim", "adm", "apdm"][usize::from(other.to_u8() >> 4)];
                let f = ["eif", "adf", "apdf"][usize::from(other.to_u8() & 0x0F)];
                format!("{m}_{f}")
            }
        }
    }
}

/// What one side tells the hub about how its game socket is reached.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReachOffer {
    /// The game socket's candidates (host, mapped, server-reflexive), at most
    /// [`crate::nat::NAT_MAX_CANDIDATES`].
    pub candidates: Vec<Candidate>,
    /// The side's NAT behavior, when it knows it.
    pub nat: Option<NatClass>,
    /// The relay the owner's game socket registered with (the fallback; the owner only).
    pub relay: Option<SocketAddr>,
    /// The relay's join code for the owner's game socket: the token the relay allocation needs
    /// (N113; the owner only).
    pub join_code: Option<String>,
    /// The owner's join ticket bytes (`server_host::JoinTicket`: the game port and the server key
    /// the member pins; the owner only).
    pub ticket: Vec<u8>,
}

impl ReachOffer {
    /// The first mapped candidate with a usable address.
    pub fn mapped(&self) -> Option<SocketAddr> {
        self.candidates
            .iter()
            .find(|c| c.kind == CandidateKind::Mapped && !c.addr.ip().is_unspecified() && c.addr.port() != 0)
            .map(|c| c.addr)
    }
}

/// What the hub sends each side once the owner approved a member's ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinPlan {
    /// The lobby.
    pub lobby: LobbyId,
    /// The other side: the owner (for the member) or the member (for the owner).
    pub peer: MemberId,
    /// The other side's player key (as its hub transport proved it).
    pub peer_identity: PlayerKey,
    /// The path to try first (see the module docs for the order).
    pub path: ReachKind,
    /// The mapped address, for [`ReachKind::Mapped`].
    pub mapped: Option<SocketAddr>,
    /// The other side's candidates.
    pub candidates: Vec<Candidate>,
    /// The relay (the fallback of a punch, the path of a relay plan).
    pub relay: Option<SocketAddr>,
    /// The relay's join code for the owner's game socket.
    pub join_code: Option<String>,
    /// The owner's join ticket bytes (empty in the owner's own copy).
    pub ticket: Vec<u8>,
}

impl JoinPlan {
    /// The server key the member pins and the ticket's address, from the ticket's bytes
    /// (`"RGJT" | version 1 | addr | has_key u8 | key [32]`, `server_host::JoinTicket`); `None` for
    /// bytes that are not a ticket.
    pub fn ticket_parts(&self) -> Option<(SocketAddr, Option<ServerKey>)> {
        let mut r = crate::wire::WireReader::new(&self.ticket);
        if r.array::<4>().ok()? != *b"RGJT" || r.u8().ok()? != 1 {
            return None;
        }
        let addr = crate::handshake::decode_addr(&mut r).ok()?;
        let key = match r.u8().ok()? {
            0 => None,
            1 => Some(ServerKey(r.array().ok()?)),
            _ => return None,
        };
        r.finish().ok()?;
        Some((addr, key))
    }

    /// The server key the member pins, from the ticket.
    pub fn server_key(&self) -> Option<ServerKey> {
        self.ticket_parts().and_then(|(_, key)| key)
    }
}

/// One member's join as the hub saw it (`lobby.hub_join_status`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRow {
    /// The lobby.
    pub lobby: LobbyId,
    /// The member.
    pub member: MemberId,
    /// The member's handle.
    pub handle: String,
    /// The member's player key, short form.
    pub identity: String,
    /// The member's `platform` member data, when set.
    pub platform: Option<String>,
    /// The member's NAT behavior name, when it told the hub.
    pub nat: Option<String>,
    /// `asked` (waiting for the owner), `planned` (the plan went out), `reached`, or `denied`.
    pub state: String,
    /// The path the broker planned.
    pub planned: Option<ReachKind>,
    /// The path that carried the member's handshake, as it reported it.
    pub chosen: Option<ReachKind>,
    /// Connectivity checks the member sent before its path was chosen.
    pub punch_attempts: u32,
    /// Whether the relay carries the member's session.
    pub relayed: bool,
}

/// Why the broker could not plan a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerError {
    /// The owner offered nothing to reach.
    NoReach,
}

/// Chooses a member's path to the owner (see the module docs for the order).
///
/// # Errors
///
/// [`BrokerError::NoReach`] when the owner offered no mapping, no candidate and no relay.
pub fn broker_reach(owner: &ReachOffer, member_nat: Option<NatClass>) -> Result<ReachKind, BrokerError> {
    if owner.mapped().is_some() {
        return Ok(ReachKind::Mapped);
    }
    let relay = owner.relay.is_some() && owner.join_code.is_some();
    if let (Some(host), Some(client)) = (owner.nat, member_nat)
        && predict(host, client) == PunchResult::NeedsRelay
        && relay
    {
        return Ok(ReachKind::Relay);
    }
    if relay || !owner.candidates.is_empty() {
        return Ok(ReachKind::Punch);
    }
    Err(BrokerError::NoReach)
}

/// What the RFC 4787 model says two NAT behaviors can do without the relay. Two sides that both map
/// per address and port never find a pair whose answer returns from where the check went
/// (RFC 8445 § 7.2.5.2.1), so they need the relay whatever their filters.
pub fn predict(host: NatClass, client: NatClass) -> PunchResult {
    if host.mapping == NatMapping::AddressAndPortDependent
        && client.mapping == NatMapping::AddressAndPortDependent
    {
        return PunchResult::NeedsRelay;
    }
    // Documentation addresses (RFC 5737): the model only needs distinct public IPs and a relay.
    let mut h = SimNat::new(host.mapping, host.filtering, [198, 51, 100, 7].into());
    let mut c = SimNat::new(client.mapping, client.filtering, [203, 0, 113, 10].into());
    simulate_hole_punch(&mut h, &mut c, SocketAddr::from(([192, 0, 2, 1], 7800)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay() -> SocketAddr {
        SocketAddr::from(([192, 0, 2, 1], 7800))
    }

    fn offer(candidates: Vec<Candidate>, nat: Option<NatClass>) -> ReachOffer {
        ReachOffer {
            candidates,
            nat,
            relay: Some(relay()),
            join_code: Some("ABCDEF".into()),
            ticket: Vec::new(),
        }
    }

    /// Acceptance (NL-05): a port mapping wins over punching and the relay, even when both sides
    /// are symmetric (DO27 `mapped_path_beats_punch_and_relay`, as requirements).
    #[test]
    fn mapped_path_beats_punch_and_relay() {
        let mapped = SocketAddr::from(([203, 0, 113, 8], 7777));
        let o = offer(
            vec![
                Candidate::new(CandidateKind::Host, SocketAddr::from(([10, 0, 0, 2], 7777))),
                Candidate::new(CandidateKind::Mapped, mapped),
            ],
            Some(NatClass::SYMMETRIC),
        );
        assert_eq!(o.mapped(), Some(mapped));
        assert_eq!(broker_reach(&o, Some(NatClass::SYMMETRIC)), Ok(ReachKind::Mapped));
        // An empty mapping is not a path: the symmetric pair goes to the relay.
        let empty = offer(
            vec![Candidate::new(
                CandidateKind::Mapped,
                SocketAddr::from(([0, 0, 0, 0], 0)),
            )],
            Some(NatClass::SYMMETRIC),
        );
        assert_eq!(
            broker_reach(&empty, Some(NatClass::SYMMETRIC)),
            Ok(ReachKind::Relay)
        );
    }

    /// A cone owner and a port-restricted member can punch; unknown NATs punch with the fallback.
    #[test]
    fn full_cone_and_unknown_nats_punch() {
        let o = offer(Vec::new(), Some(NatClass::FULL_CONE));
        assert_eq!(
            broker_reach(&o, Some(NatClass::PORT_RESTRICTED)),
            Ok(ReachKind::Punch)
        );
        assert_eq!(broker_reach(&o, None), Ok(ReachKind::Punch));
        let unknown = offer(Vec::new(), None);
        assert_eq!(
            broker_reach(&unknown, Some(NatClass::SYMMETRIC)),
            Ok(ReachKind::Punch)
        );
        let nothing = ReachOffer::default();
        assert_eq!(broker_reach(&nothing, None), Err(BrokerError::NoReach));
    }

    /// Acceptance (NL-05): a symmetric member behind a port-restricted owner, or symmetric on both
    /// sides, goes to the relay at once (DO27 `symmetric_nat_falls_back_to_relay_bind`).
    #[test]
    fn symmetric_nat_plans_the_relay() {
        let o = offer(Vec::new(), Some(NatClass::PORT_RESTRICTED));
        assert_eq!(broker_reach(&o, Some(NatClass::SYMMETRIC)), Ok(ReachKind::Relay));
        let s = offer(Vec::new(), Some(NatClass::SYMMETRIC));
        assert_eq!(broker_reach(&s, Some(NatClass::SYMMETRIC)), Ok(ReachKind::Relay));
        // Without a relay token there is no relay plan: punching is all that is left.
        let mut no_relay = s.clone();
        no_relay.join_code = None;
        no_relay.candidates = vec![Candidate::new(
            CandidateKind::ServerReflexive,
            SocketAddr::from(([198, 51, 100, 7], 40_000)),
        )];
        assert_eq!(
            broker_reach(&no_relay, Some(NatClass::SYMMETRIC)),
            Ok(ReachKind::Punch)
        );
    }

    #[test]
    fn nat_classes_round_trip_their_byte_and_name() {
        for m in NatMapping::ALL {
            for f in NatFiltering::ALL {
                let c = NatClass {
                    mapping: m,
                    filtering: f,
                };
                assert_eq!(NatClass::from_u8(c.to_u8()), Some(c));
                assert!(c.name().bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
            }
        }
        assert_eq!(NatClass::from_u8(0x30), None);
        assert_eq!(NatClass::SYMMETRIC.name(), "symmetric");
        for k in [ReachKind::Mapped, ReachKind::Punch, ReachKind::Relay] {
            assert_eq!(ReachKind::from_u8(k.to_u8()), Some(k));
        }
    }
}
