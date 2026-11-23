// SPDX-License-Identifier: MPL-2.0
//! [`HubReach`]: a member runs its [`JoinPlan`] on its game socket (NL-05, ADR-8607).
//!
//! - [`ReachKind::Mapped`]: the owner's port mapping is dialed directly; nothing else runs.
//! - [`ReachKind::Punch`]: `server_host::JoinPath` on the plan's relay and join code: the relay's
//!   introduction (protocol 3) starts paced connectivity checks from both game sockets, the best
//!   answered pair carries the handshake, and the relay allocation wins at 1.5 s when nothing else
//!   completed.
//! - [`ReachKind::Relay`]: `JoinPath::relay_first`: the allocation at once, no checks.
//!
//! The server key the endpoint pins is the one in the owner's ticket that came over the
//! authenticated hub link. A relay whose ticket names another key fails the reach with
//! `hub_key_mismatch` before the endpoint sends anything.

use std::io;
use std::net::SocketAddr;

use super::HubError;
use super::join::{JoinPlan, ReachKind};
use crate::crypto::ServerKey;
use crate::server_host::{JoinPath, JoinRoute, JoinSocket};
use crate::socket::DatagramSocket;

/// The socket the member's game endpoint uses once the reach is ready.
pub enum ReachSocket<S> {
    /// The plain game socket (a mapped path).
    Direct(S),
    /// The join path's socket (punch or relay).
    Path(JoinSocket<S>),
}

impl<S> std::fmt::Debug for ReachSocket<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct(_) => f.write_str("ReachSocket::Direct"),
            Self::Path(_) => f.write_str("ReachSocket::Path"),
        }
    }
}

impl<S: DatagramSocket> DatagramSocket for ReachSocket<S> {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        match self {
            Self::Direct(s) => s.send_to(bytes, dest),
            Self::Path(s) => s.send_to(bytes, dest),
        }
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        match self {
            Self::Direct(s) => s.recv_from(buf),
            Self::Path(s) => s.recv_from(buf),
        }
    }

    fn max_datagram_size(&self, dest: SocketAddr) -> usize {
        match self {
            Self::Direct(s) => s.max_datagram_size(dest),
            Self::Path(s) => s.max_datagram_size(dest),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Direct(s) => s.local_addr(),
            Self::Path(s) => s.local_addr(),
        }
    }
}

/// Where a [`HubReach`] is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReachState {
    /// Resolving the relay's ticket.
    Dialing,
    /// The socket is ready ([`HubReach::take_socket`]); the path may still change (the relay can
    /// win a punch race).
    Ready,
    /// The reach failed; the code says why (`hub_key_mismatch`, or a `JoinError` code).
    Failed(String),
}

enum Way<S> {
    Mapped { socket: Option<S>, addr: SocketAddr },
    Path(Box<JoinPath<S>>),
}

/// A member's reach toward the owner's game session, from a hub [`JoinPlan`].
pub struct HubReach<S> {
    plan: JoinPlan,
    way: Way<S>,
    state: ReachState,
    taken: bool,
}

impl<S> std::fmt::Debug for HubReach<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubReach")
            .field("lobby", &self.plan.lobby)
            .field("path", &self.plan.path)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl<S: DatagramSocket> HubReach<S> {
    /// Starts `plan` on `socket` (the socket the game endpoint will use). `client_key` is the
    /// member's secret relay allocation key (`server_host::fresh_allocation_key`).
    ///
    /// # Errors
    ///
    /// [`HubError::ReachInvalid`] for a mapped plan without an address, or a punch or relay plan
    /// without a relay and a valid join code.
    pub fn new(
        socket: S,
        plan: &JoinPlan,
        client_key: [u8; crate::relay::RELAY_ALLOCATION_KEY_BYTES],
    ) -> Result<Self, HubError> {
        let way = match plan.path {
            ReachKind::Mapped => Way::Mapped {
                socket: Some(socket),
                addr: plan.mapped.ok_or(HubError::ReachInvalid)?,
            },
            ReachKind::Punch | ReachKind::Relay => {
                let (Some(relay), Some(code)) = (plan.relay, plan.join_code.as_deref()) else {
                    return Err(HubError::ReachInvalid);
                };
                let path =
                    JoinPath::new(socket, relay, code, client_key).map_err(|_| HubError::ReachInvalid)?;
                Way::Path(Box::new(if plan.path == ReachKind::Relay {
                    path.relay_first()
                } else {
                    path
                }))
            }
        };
        let state = match way {
            Way::Mapped { .. } => ReachState::Ready,
            Way::Path(_) => ReachState::Dialing,
        };
        Ok(Self {
            plan: plan.clone(),
            way,
            state,
            taken: false,
        })
    }

    /// The plan.
    pub fn plan(&self) -> &JoinPlan {
        &self.plan
    }

    /// The state.
    pub fn state(&self) -> &ReachState {
        &self.state
    }

    /// The join path (punch and relay plans).
    pub fn join_path(&self) -> Option<&JoinPath<S>> {
        match &self.way {
            Way::Path(p) => Some(p),
            Way::Mapped { .. } => None,
        }
    }

    /// Runs the path's relay control and checks. Call it every frame before the endpoint's
    /// receive, until the reach is done.
    pub fn update(&mut self, now_ms: u64) -> &ReachState {
        let Way::Path(path) = &mut self.way else {
            return &self.state;
        };
        if matches!(self.state, ReachState::Failed(_)) {
            return &self.state;
        }
        let route = path.update(now_ms);
        if route == JoinRoute::Failed {
            let code = path.error().map_or("join_failed", |e| e.code());
            self.state = ReachState::Failed(code.to_owned());
        } else if self.state == ReachState::Dialing
            && let Some(ticket) = path.ticket()
        {
            // The owner's ticket came over the hub link; the relay's must name the same key, and
            // must name none when the owner's names none: the relay chooses the address, never the
            // key the endpoint pins (ADR-8665).
            self.state = if ticket.server_key == self.plan.server_key() {
                ReachState::Ready
            } else {
                ReachState::Failed(HubError::KeyMismatch.code().to_owned())
            };
        }
        &self.state
    }

    /// The socket, the address to connect the endpoint to, and the key to pin, once
    /// [`ReachState::Ready`] (once). The key is always the plan's (the owner's ticket from the hub
    /// link), never one only the relay named.
    pub fn take_socket(&mut self) -> Option<(ReachSocket<S>, SocketAddr, Option<ServerKey>)> {
        if self.state != ReachState::Ready || self.taken {
            return None;
        }
        let pin = self.plan.server_key();
        let out = match &mut self.way {
            Way::Mapped { socket, addr } => (ReachSocket::Direct(socket.take()?), *addr, pin),
            Way::Path(path) => {
                let ticket = path.ticket()?.clone();
                let socket = path.take_socket()?;
                (ReachSocket::Path(socket), ticket.addr, pin)
            }
        };
        self.taken = true;
        Some(out)
    }

    /// The path that carries the session and the connectivity checks sent, once it is settled:
    /// at once for a mapped plan; for a join path once the direct pair was chosen or the relay won.
    /// Report it with `HubLobbyBackend::report_reach` after the endpoint connected.
    pub fn outcome(&self) -> Option<(ReachKind, u32)> {
        match &self.way {
            Way::Mapped { .. } => Some((ReachKind::Mapped, 0)),
            Way::Path(path) => {
                let attempts = path
                    .checks()
                    .map_or(0, |c| u32::try_from(c.counts().requests_sent).unwrap_or(u32::MAX));
                match path.route() {
                    JoinRoute::DirectChosen => Some((ReachKind::Punch, attempts)),
                    JoinRoute::Relayed(_) => Some((ReachKind::Relay, attempts)),
                    _ => None,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::{HubReach, ReachState};
    use crate::crypto::{PlayerKey, ServerIdentity, ServerKey};
    use crate::lobby::hub::join::{JoinPlan, ReachKind};
    use crate::lobby::{LobbyId, MemberId};
    use crate::relay::{RELAY_ALLOCATION_KEY_BYTES, RelayServer};
    use crate::server_host::{JoinTicket, RelayRegistrar};
    use crate::socket::{LoopbackNet, LoopbackSocket};

    struct Registered {
        net: LoopbackNet,
        relay: RelayServer<LoopbackSocket>,
        registrar: RelayRegistrar,
        relay_addr: SocketAddr,
        code: String,
        key: ServerKey,
        now: u64,
        next_port: u16,
    }

    impl Registered {
        fn new() -> Self {
            let net = LoopbackNet::new();
            let relay_addr = SocketAddr::from(([192, 0, 2, 1], 7800));
            let relay = RelayServer::new(net.bind(relay_addr).expect("bind relay"), 1);
            let key = ServerIdentity::from_private([6; 32]).expect("curve").public();
            let ticket = JoinTicket::from_game_socket(Some(key));
            let registrar = RelayRegistrar::new(
                Box::new(
                    net.bind(SocketAddr::from(([192, 0, 2, 10], 7777)))
                        .expect("bind host"),
                ),
                relay_addr,
                &ticket,
            )
            .expect("ticket fits");
            let mut reg = Self {
                net,
                relay,
                registrar,
                relay_addr,
                code: String::new(),
                key,
                now: 0,
                next_port: 5000,
            };
            let mut code = None;
            for _ in 0..500 {
                reg.registrar.update(reg.now);
                reg.relay.update(reg.now);
                reg.now += 20;
                if let Some(got) = reg.registrar.join_code() {
                    code = Some(got.to_owned());
                    break;
                }
            }
            reg.code = code.expect("the relay issued a join code");
            reg
        }

        fn plan(&self, ticket_key: Option<ServerKey>, path: ReachKind) -> JoinPlan {
            let ticket = JoinTicket {
                addr: SocketAddr::from(([198, 51, 100, 7], 7777)),
                server_key: ticket_key,
            };
            JoinPlan {
                lobby: LobbyId(1),
                peer: MemberId(1),
                peer_identity: PlayerKey([1; 32]),
                path,
                mapped: None,
                candidates: Vec::new(),
                relay: Some(self.relay_addr),
                join_code: Some(self.code.clone()),
                ticket: ticket.encode(),
            }
        }

        fn reach(&mut self, ticket_key: Option<ServerKey>, path: ReachKind) -> HubReach<LoopbackSocket> {
            let plan = self.plan(ticket_key, path);
            let port = self.next_port;
            self.next_port += 1;
            let socket = self
                .net
                .bind(SocketAddr::from(([192, 0, 2, 20], port)))
                .expect("bind member");
            HubReach::new(socket, &plan, [9; RELAY_ALLOCATION_KEY_BYTES]).expect("plan")
        }

        fn settle(&mut self, reach: &mut HubReach<LoopbackSocket>) -> ReachState {
            for _ in 0..500 {
                self.registrar.update(self.now);
                self.relay.update(self.now);
                let state = reach.update(self.now).clone();
                if !matches!(state, ReachState::Dialing) {
                    return state;
                }
                self.now += 20;
            }
            reach.state().clone()
        }
    }

    /// The relay's ticket must name the key in the hub plan, on a punch and on `relay_first`.
    #[test]
    fn a_relay_ticket_for_another_key_is_hub_key_mismatch() {
        let mut reg = Registered::new();
        let key = reg.key;
        let other = ServerIdentity::from_private([7; 32]).expect("curve").public();
        for path in [ReachKind::Punch, ReachKind::Relay] {
            let mut same = reg.reach(Some(key), path);
            assert_eq!(reg.settle(&mut same), ReachState::Ready, "{path:?} matching key");
            let taken = same.take_socket().expect("ready socket");
            assert_eq!(taken.2, Some(key), "{path:?} pins the plan's key");

            let mut other_reach = reg.reach(Some(other), path);
            assert_eq!(
                reg.settle(&mut other_reach),
                ReachState::Failed("hub_key_mismatch".into()),
                "{path:?} relay key does not match the plan"
            );
            assert!(other_reach.take_socket().is_none(), "{path:?}");
        }
    }

    /// A plan whose ticket names no server key must not adopt whatever key the relay returns.
    /// Asserts the safe property: it fails if `take_socket` pins the relay's key.
    #[test]
    fn a_plan_without_a_server_key_does_not_adopt_the_relay_key() {
        let mut reg = Registered::new();
        for path in [ReachKind::Punch, ReachKind::Relay] {
            let mut reach = reg.reach(None, path);
            let state = reg.settle(&mut reach);
            match state {
                ReachState::Failed(code) => {
                    assert_eq!(code, "hub_key_mismatch", "{path:?}");
                    assert!(reach.take_socket().is_none());
                }
                ReachState::Ready => {
                    let taken = reach.take_socket().expect("ready");
                    assert_eq!(
                        taken.2, None,
                        "{path:?} pinned {:?} from a plan that named no key",
                        taken.2
                    );
                }
                ReachState::Dialing => panic!("{path:?} did not settle"),
            }
        }
    }
}
