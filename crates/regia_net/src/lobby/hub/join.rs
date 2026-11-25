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
//! When `relay_only` is set on the call, on the owner's offer or on the member's offer, the only
//! success path is the relay (spec 89 § 16.4, ADR-10391). Neither plan carries a candidate or a
//! mapped address, and the member's ticket is rewritten to an unspecified address with the same
//! server key. A listen exchange with the flag clear keeps the order above.
//!
//! The order, the mapped-first rule and the punch-or-relay prediction follow DO27's `ir-hub`
//! `join.rs` as requirements only (`docs/do27-port-review.md` § 2); no line is copied.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use super::binding::ServerPresence;
use super::{HubError, HubServer};
use crate::crypto::{PlayerKey, ServerKey};
use crate::lobby::{LobbyId, LobbySpec, MemberId};
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
///
/// `non_exhaustive`: a struct pattern in another crate needs `..`. Another crate
/// cannot use a struct literal. Build one with `ReachOffer::default()` and assign
/// the fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
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
    /// When set, this side offers only the relay. Omitted on the wire when false, so a version-1
    /// offer is unchanged (ADR-10378).
    pub relay_only: bool,
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
/// An owner offer with [`ReachOffer::relay_only`] returns [`ReachKind::Relay`] when it has a
/// relay and a join code, and [`BrokerError::NoReach`] otherwise. It does not return a mapped or
/// punch path. The member's own flag and the lobby flag are applied by [`plans_for`].
///
/// # Errors
///
/// [`BrokerError::NoReach`] when the owner offered no mapping, no candidate and no relay, or
/// offered only the relay without a relay and a join code.
pub fn broker_reach(owner: &ReachOffer, member_nat: Option<NatClass>) -> Result<ReachKind, BrokerError> {
    let relay = owner.relay.is_some() && owner.join_code.is_some();
    if owner.relay_only {
        return if relay {
            Ok(ReachKind::Relay)
        } else {
            Err(BrokerError::NoReach)
        };
    }
    if owner.mapped().is_some() {
        return Ok(ReachKind::Mapped);
    }
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

/// Registers a dedicated server on `member`'s session.
///
/// # Errors
///
/// The codes in spec 89 § 16.3. One bad owner proof fails the whole call.
pub fn register_server(
    server: &mut HubServer,
    member: MemberId,
    presence: &ServerPresence,
    now_ms: u64,
) -> Result<(), HubError> {
    server.register_server(member, presence, now_ms)
}

/// Revokes the binding `(member's player key, server_key, serial)`.
///
/// # Errors
///
/// [`HubError::NotAPlayer`] when the session has no player key, [`HubError::RevokeFull`] at the cap.
pub fn revoke_binding(
    server: &mut HubServer,
    member: MemberId,
    server_key: ServerKey,
    serial: u64,
    now_ms: u64,
) -> Result<(), HubError> {
    server.revoke_binding(member, server_key, serial, now_ms)
}

/// Creates a delegated lobby owned by `member` and hosted by `server_key`.
///
/// # Errors
///
/// The create rows of spec 89 § 16.3.
pub fn create_delegated(
    server: &mut HubServer,
    member: MemberId,
    spec: &LobbySpec,
    server_key: ServerKey,
    now_ms: u64,
) -> Result<LobbyId, HubError> {
    server.create_delegated(member, spec, server_key, now_ms)
}

/// Sets whether `lobby` offers only the relay (spec 89 § 16.4).
///
/// The listen owner, the delegated lobby's owning player or its bound server, or the
/// ownerless lobby's server session may set or clear the flag. Every member is then sent
/// the room, and a listed lobby's directory card updates. `approve` applies the stored
/// flag to every member's exchange.
///
/// # Errors
///
/// [`HubError::NoSession`] when `member` has no session. [`crate::lobby::LobbyError::NotInLobby`]
/// when the lobby is missing or a listen caller is not in it. [`crate::lobby::LobbyError::NotOwner`]
/// when a listen or ownerless caller may not change it. [`HubError::BindingNotOwner`] when a
/// delegated caller is neither the owning player nor the bound server.
pub fn set_reach_mode(
    server: &mut HubServer,
    member: MemberId,
    lobby: LobbyId,
    relay_only: bool,
    now_ms: u64,
) -> Result<(), HubError> {
    server.set_reach_mode(member, lobby, relay_only, now_ms)
}

/// Replaces `lobby`'s ban list with `keys`.
///
/// # Errors
///
/// [`HubError::NotReady`] until the behavior lane lands.
pub fn set_bans(
    server: &mut HubServer,
    member: MemberId,
    lobby: LobbyId,
    keys: &[PlayerKey],
    now_ms: u64,
) -> Result<(), HubError> {
    let _ = (server, member, lobby, keys, now_ms);
    Err(HubError::NotReady)
}

/// Closes `lobby`. Listen-lobby close stays on the existing leave path and returns
/// [`HubError::NotReady`].
///
/// # Errors
///
/// [`HubError::NotReady`] for a listen lobby, [`HubError::BindingNotOwner`] when the caller is not
/// the delegated owner.
pub fn close_lobby(
    server: &mut HubServer,
    member: MemberId,
    lobby: LobbyId,
    now_ms: u64,
) -> Result<(), HubError> {
    server.close_lobby(member, lobby, now_ms)
}

/// The member's plan and the owner's plan for one approved ask.
///
/// With the flag clear, the order is today's: mapped, then punch, then relay ([`broker_reach`]).
/// The member's plan carries the owner's candidates, mapped address, relay, join code and ticket.
/// The owner's plan carries the member's candidates, no mapped address and an empty ticket.
///
/// `relay_only`, [`ReachOffer::relay_only`] on the owner, or [`ReachOffer::relay_only`] on the
/// member hides that exchange (spec 89 § 16.4). The path is [`ReachKind::Relay`] when the owner
/// offered a relay and a join code, and [`BrokerError::NoReach`] otherwise. Neither plan carries
/// a candidate or a mapped address. The member's ticket is rewritten to
/// `JoinTicket::from_game_socket` (unspecified address, port 0, the same server key). The relay
/// address and the join code stay on the member's plan. The owner's ticket stays empty.
///
/// # Errors
///
/// [`BrokerError::NoReach`] when the owner offered nothing the chosen path can use.
#[allow(clippy::too_many_arguments)] // ADR-10378: frozen signature, eight arguments.
pub fn plans_for(
    owner_offer: &ReachOffer,
    member_offer: &ReachOffer,
    relay_only: bool,
    lobby: LobbyId,
    owner: MemberId,
    owner_identity: PlayerKey,
    member: MemberId,
    member_identity: PlayerKey,
) -> Result<(JoinPlan, JoinPlan), BrokerError> {
    let hide = relay_only || owner_offer.relay_only || member_offer.relay_only;
    if hide && (owner_offer.relay.is_none() || owner_offer.join_code.is_none()) {
        return Err(BrokerError::NoReach);
    }
    let path = if hide {
        ReachKind::Relay
    } else {
        broker_reach(owner_offer, member_offer.nat)?
    };
    let for_member = JoinPlan {
        lobby,
        peer: owner,
        peer_identity: owner_identity,
        path,
        mapped: (!hide && path == ReachKind::Mapped)
            .then(|| owner_offer.mapped())
            .flatten(),
        candidates: if hide {
            Vec::new()
        } else {
            owner_offer.candidates.clone()
        },
        relay: owner_offer.relay,
        join_code: owner_offer.join_code.clone(),
        ticket: if hide {
            unspecified_ticket(&owner_offer.ticket)
        } else {
            owner_offer.ticket.clone()
        },
    };
    let for_owner = JoinPlan {
        peer: member,
        peer_identity: member_identity,
        mapped: None,
        candidates: if hide {
            Vec::new()
        } else {
            member_offer.candidates.clone()
        },
        ticket: Vec::new(),
        ..for_member.clone()
    };
    Ok((for_member, for_owner))
}

/// The member's ticket with the address removed and the server key kept (spec 89 § 16.4).
///
/// Bytes that are not a ticket still become an unspecified ticket, so a relay-only plan never
/// copies an address blob through.
fn unspecified_ticket(bytes: &[u8]) -> Vec<u8> {
    let key = crate::server_host::JoinTicket::decode(bytes)
        .ok()
        .and_then(|ticket| ticket.server_key);
    crate::server_host::JoinTicket::from_game_socket(key).encode()
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
    use crate::lobby::hub::HubEvent;

    fn relay() -> SocketAddr {
        SocketAddr::from(([192, 0, 2, 1], 7800))
    }

    fn has_ip(bytes: &[u8], addr: SocketAddr) -> bool {
        let std::net::IpAddr::V4(ip) = addr.ip() else {
            return false;
        };
        bytes.windows(4).any(|window| window == ip.octets())
    }

    fn offer(candidates: Vec<Candidate>, nat: Option<NatClass>) -> ReachOffer {
        ReachOffer {
            candidates,
            nat,
            relay: Some(relay()),
            join_code: Some("ABCDEF".into()),
            ticket: Vec::new(),
            relay_only: false,
        }
    }

    /// `relay_only` drops both candidate lists and rewrites the member's ticket to an unspecified
    /// address with the same server key, even when the owner has a mapped candidate.
    #[test]
    fn plans_for_honors_relay_only() {
        let host: SocketAddr = "198.51.100.7:7777".parse().expect("host");
        let mapped: SocketAddr = "198.51.100.50:7778".parse().expect("mapped");
        let member_addr: SocketAddr = "203.0.113.20:5000".parse().expect("member");
        let key = ServerKey([0x11; 32]);
        let ticket = crate::server_host::JoinTicket {
            addr: host,
            server_key: Some(key),
        };
        let mut owner_offer = offer(
            vec![
                Candidate::new(CandidateKind::Host, host),
                Candidate::new(CandidateKind::Mapped, mapped),
            ],
            Some(NatClass::FULL_CONE),
        );
        owner_offer.ticket = ticket.encode();
        let member_offer = ReachOffer {
            candidates: vec![Candidate::new(CandidateKind::Host, member_addr)],
            nat: Some(NatClass::PORT_RESTRICTED),
            ..ReachOffer::default()
        };
        let (for_member, for_owner) = plans_for(
            &owner_offer,
            &member_offer,
            true,
            LobbyId(7),
            MemberId(2),
            PlayerKey([2; 32]),
            MemberId(3),
            PlayerKey([3; 32]),
        )
        .expect("relay plan");
        assert_eq!(for_member.path, ReachKind::Relay);
        assert!(for_member.candidates.is_empty());
        assert!(for_member.mapped.is_none());
        assert_eq!(for_member.relay, Some(relay()));
        assert_eq!(for_member.join_code.as_deref(), Some("ABCDEF"));
        let (addr, got_key) = for_member.ticket_parts().expect("rewritten ticket");
        assert!(addr.ip().is_unspecified() && addr.port() == 0);
        assert_eq!(got_key, Some(key));
        assert!(for_owner.candidates.is_empty());
        assert!(for_owner.ticket.is_empty());
        assert_eq!(for_owner.peer, MemberId(3));
        assert_eq!(for_owner.path, ReachKind::Relay);
        let member_bytes = HubEvent::Plan(for_member).to_bytes().expect("member plan bytes");
        let owner_bytes = HubEvent::Plan(for_owner).to_bytes().expect("owner plan bytes");
        assert!(!has_ip(&member_bytes, host));
        assert!(!has_ip(&member_bytes, mapped));
        assert!(!has_ip(&owner_bytes, member_addr));
    }

    /// The member's own flag hides that exchange. The owner's mapped offer still plans a mapped
    /// path for a member who did not set the flag.
    #[test]
    fn one_members_relay_only_does_not_hide_the_other() {
        let host: SocketAddr = "198.51.100.7:7777".parse().expect("host");
        let mapped: SocketAddr = "198.51.100.50:7778".parse().expect("mapped");
        let hidden: SocketAddr = "203.0.113.20:5000".parse().expect("hidden");
        let open: SocketAddr = "203.0.113.21:5001".parse().expect("open");
        let mut owner_offer = offer(
            vec![
                Candidate::new(CandidateKind::Host, host),
                Candidate::new(CandidateKind::Mapped, mapped),
            ],
            Some(NatClass::FULL_CONE),
        );
        owner_offer.ticket = crate::server_host::JoinTicket {
            addr: host,
            server_key: Some(ServerKey([0x11; 32])),
        }
        .encode();
        let mut hiding = ReachOffer {
            candidates: vec![Candidate::new(CandidateKind::Host, hidden)],
            relay_only: true,
            ..ReachOffer::default()
        };
        hiding.nat = Some(NatClass::PORT_RESTRICTED);
        let open_offer = ReachOffer {
            candidates: vec![Candidate::new(CandidateKind::Host, open)],
            nat: Some(NatClass::PORT_RESTRICTED),
            ..ReachOffer::default()
        };
        let (hidden_member, hidden_owner) = plans_for(
            &owner_offer,
            &hiding,
            false,
            LobbyId(1),
            MemberId(2),
            PlayerKey([2; 32]),
            MemberId(3),
            PlayerKey([3; 32]),
        )
        .expect("hidden");
        assert_eq!(hidden_member.path, ReachKind::Relay);
        assert!(hidden_member.candidates.is_empty());
        assert!(hidden_owner.candidates.is_empty());
        let (open_member, open_owner) = plans_for(
            &owner_offer,
            &open_offer,
            false,
            LobbyId(1),
            MemberId(2),
            PlayerKey([2; 32]),
            MemberId(4),
            PlayerKey([4; 32]),
        )
        .expect("open");
        assert_eq!(open_member.path, ReachKind::Mapped);
        assert_eq!(open_member.mapped, Some(mapped));
        assert_eq!(open_member.candidates, owner_offer.candidates);
        assert_eq!(open_owner.candidates, open_offer.candidates);
        let hidden_bytes = HubEvent::Plan(hidden_member).to_bytes().expect("bytes");
        let owner_bytes = HubEvent::Plan(hidden_owner).to_bytes().expect("bytes");
        assert!(!has_ip(&hidden_bytes, host));
        assert!(!has_ip(&hidden_bytes, mapped));
        assert!(!has_ip(&owner_bytes, hidden));
    }

    /// A relay-only offer with no relay and no join code is not a punch.
    #[test]
    fn relay_only_without_a_relay_is_no_reach() {
        let mut owner_offer = offer(
            vec![Candidate::new(
                CandidateKind::Host,
                "198.51.100.7:7777".parse().expect("host"),
            )],
            Some(NatClass::FULL_CONE),
        );
        owner_offer.relay_only = true;
        owner_offer.join_code = None;
        assert_eq!(
            broker_reach(&owner_offer, Some(NatClass::FULL_CONE)),
            Err(BrokerError::NoReach)
        );
        assert_eq!(
            plans_for(
                &owner_offer,
                &ReachOffer::default(),
                false,
                LobbyId(1),
                MemberId(2),
                PlayerKey([2; 32]),
                MemberId(3),
                PlayerKey([3; 32]),
            ),
            Err(BrokerError::NoReach)
        );
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
