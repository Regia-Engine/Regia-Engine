// SPDX-License-Identifier: MPL-2.0
//! [`HubHost`]: a [`HubServer`] behind a [`NetServer`] that always seals with the hub's identity.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use super::limits::AdmitGate;
use super::wire::{self, HubMessage, HubRequest, HubWire};
use super::{HubError, HubEvent, HubLimits, HubReport, HubServer, HubSettings};
use crate::connection::{Channel, SendError};
use crate::crypto::{CryptoMode, PlayerKey, ServerIdentity, ServerKey, verify_player_proof};
use crate::handshake::DisconnectReason;
use crate::lobby::{LobbyId, LobbySpec, MemberId};
use crate::server::{NetServer, ServerConfig, ServerEvent};
use crate::session::PeerId;
use crate::socket::DatagramSocket;

/// The refused player proof at which the hub drops a link (ADR-8734): three are answered with
/// `hub_identity_mismatch` and the link stays up (a stale proof then a fresh one, or a client
/// retrying after a mistake, still gets its session on that link); the fourth drops it. Each
/// refused proof costs the hub up to four X25519, so the cost per link stays bounded.
pub const PROOF_FAILURE_LIMIT: u8 = 4;

/// A running hub: the network endpoint and the state core. Call [`HubHost::update`] every frame
/// (or every few milliseconds in a headless hub process) with the hub's clock.
///
/// The endpoint always encrypts, loopback included ([`CryptoMode::Required`]), under the identity
/// the host was given: clients pin its public half ([`HubHost::server_key`]). A peer that completed
/// the handshake has no hub session until its `Hello`; one session per connection.
pub struct HubHost<S: DatagramSocket> {
    server: NetServer<HubWire, S>,
    core: HubServer,
    /// The hub's static key pair: player proofs are made to its public half.
    identity: ServerIdentity,
    by_peer: BTreeMap<PeerId, MemberId>,
    by_member: BTreeMap<MemberId, PeerId>,
    /// Peers that finished the handshake and have not said hello yet, with when they joined.
    unconfirmed: BTreeMap<PeerId, u64>,
    /// Per-address and per-identity request gate. It runs before the core, including before
    /// member-data writes.
    limits: AdmitGate,
    /// Set by [`HubHost::begin_drain`]: new hellos are refused and current peers are kicked.
    draining: bool,
    /// Peers whose link is dormant ([`ServerEvent::PeerDormant`], with
    /// [`ServerConfig::resume_grace_ms`] above 0): their sessions are held until the peer resumes
    /// (a resync then) or leaves.
    dormant: BTreeSet<PeerId>,
    /// Refused player proofs per link without a session (ADR-8734). At [`PROOF_FAILURE_LIMIT`]
    /// the link is dropped and its later proofs are not checked.
    proof_failures: BTreeMap<PeerId, u8>,
    /// Codec generation the peer announced. Missing means wire 1 (a v0.7.0 client). ADR-10378.
    peer_wire: BTreeMap<PeerId, u8>,
}

impl<S: DatagramSocket> std::fmt::Debug for HubHost<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubHost")
            .field("core", &self.core)
            .field("peers", &self.by_peer.len())
            .finish_non_exhaustive()
    }
}

impl<S: DatagramSocket> HubHost<S> {
    /// A hub on `socket` with `identity` (its Noise static key) and `settings`.
    pub fn new(socket: S, identity: ServerIdentity, settings: HubSettings) -> Self {
        Self::with_config(socket, identity, settings, ServerConfig::default())
    }

    /// As [`HubHost::new`], with the endpoint tuned by `config`. The identity, the crypto mode
    /// (always [`CryptoMode::Required`]), the peer cap (the session cap) and the per-peer send rate
    /// ([`HubSettings::reliable_msgs_per_s`]) are the hub's. A `resume_grace_ms` above 0 holds a
    /// dormant peer's session for the grace (a resync when it resumes).
    pub fn with_config(
        socket: S,
        identity: ServerIdentity,
        settings: HubSettings,
        mut config: ServerConfig,
    ) -> Self {
        let mut core = HubServer::new(settings);
        core.set_hub_identity(identity.clone());
        config.identity = Some(identity.clone());
        config.crypto = CryptoMode::Required;
        config.max_peers = core.settings().session_cap;
        // A bounded per-peer send rate (ADR-8633): what the link refuses waits in the session's
        // outbox, which is bounded by its cap.
        config.reliable_msgs_per_s = core.settings().reliable_msgs_per_s;
        Self {
            server: NetServer::new(socket, config),
            core,
            identity,
            by_peer: BTreeMap::new(),
            by_member: BTreeMap::new(),
            unconfirmed: BTreeMap::new(),
            limits: AdmitGate::default(),
            draining: false,
            dormant: BTreeSet::new(),
            proof_failures: BTreeMap::new(),
            peer_wire: BTreeMap::new(),
        }
    }

    /// The state core.
    pub fn core(&self) -> &HubServer {
        &self.core
    }

    /// The state core, mutable (operator actions, tests).
    pub fn core_mut(&mut self) -> &mut HubServer {
        &mut self.core
    }

    /// The network endpoint.
    pub fn server(&self) -> &NetServer<HubWire, S> {
        &self.server
    }

    /// The hub's public key: what clients pin ([`super::HubClientConfig::server_key`]).
    pub fn server_key(&self) -> Option<ServerKey> {
        self.server.server_key()
    }

    /// The agent report (`lobby.hub_status`).
    pub fn report(&self) -> HubReport {
        self.core.report()
    }

    /// Publishes the report into the agent bridge under `label` (`lobby.hub_status`).
    pub fn publish(&self, label: &str, state: &mut crate::status::NetAgentState) {
        state.publish_hub(label, self.core.report());
    }

    /// Replaces the per-address and per-identity gate. The core's own limits (chat, and
    /// member-data when that layer is present) are unchanged.
    pub fn set_limits(&mut self, limits: HubLimits) {
        self.limits = AdmitGate::new(limits);
    }

    /// Admits one request from `addr` under `identity` at `now_ms`. [`HubHost::update`] calls
    /// this before the core sees the request.
    ///
    /// # Errors
    ///
    /// [`HubError::AddressRateLimited`] or [`HubError::IdentityRateLimited`].
    pub fn admit(
        &mut self,
        addr: Option<IpAddr>,
        identity: Option<&PlayerKey>,
        now_ms: u64,
    ) -> Result<(), HubError> {
        self.limits.admit(addr, identity, now_ms)
    }

    /// True while the hub is draining.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.draining
    }

    /// True once a drain has no sessions and no unconfirmed peers left.
    #[must_use]
    pub fn finished_draining(&self) -> bool {
        self.draining && self.by_peer.is_empty() && self.unconfirmed.is_empty() && self.dormant.is_empty()
    }

    /// True while draining once no session is left, live or dormant. A refused link that has
    /// not left yet may still be unconfirmed ([`Self::finished_draining`] waits for it too).
    #[must_use]
    pub(super) fn sessions_gone(&self) -> bool {
        self.draining && self.by_peer.is_empty() && self.dormant.is_empty()
    }

    /// True when a new hello would be accepted.
    #[must_use]
    pub fn accepts_sessions(&self) -> bool {
        !self.draining
    }

    /// Refuses new sessions, kicks every peer with [`DisconnectReason::ServerShutdown`] and drops
    /// the hub sessions.
    pub fn begin_drain(&mut self) {
        self.draining = true;
        self.core.set_draining(true);
        let members: Vec<MemberId> = self.by_member.keys().copied().collect();
        for member in members {
            if let Some(peer) = self.by_member.remove(&member) {
                self.by_peer.remove(&peer);
                self.dormant.remove(&peer);
                self.peer_wire.remove(&peer);
                self.server.kick(peer, DisconnectReason::ServerShutdown);
            }
            self.core.disconnect(member);
        }
        let pending: Vec<PeerId> = self.unconfirmed.keys().copied().collect();
        for peer in pending {
            self.unconfirmed.remove(&peer);
            self.dormant.remove(&peer);
            self.peer_wire.remove(&peer);
            self.server.kick(peer, DisconnectReason::ServerShutdown);
        }
    }

    /// Receives, applies every request, flushes the directory, sends every queued event, then
    /// expires idle sessions and slow readers. `now_ms` is the hub's clock.
    ///
    /// The drain comes before the expiry (ADR-8633): a session that overflowed its outbox in this
    /// update and reads is resynchronized, not dropped; one whose link cannot take its events is.
    ///
    /// Address and identity limits run before the core sees a request, including a member-data
    /// write.
    pub fn update(&mut self, now_ms: u64) {
        let _span = tracing::trace_span!("lobby_hub_update").entered();
        self.server.receive(now_ms);
        while let Some(event) = self.server.poll_event() {
            match event {
                ServerEvent::PeerJoined { peer, .. } => {
                    self.unconfirmed.insert(peer, now_ms);
                }
                ServerEvent::Message { peer, message, .. } => self.on_message(peer, message, now_ms),
                ServerEvent::PeerDormant { peer, .. } => {
                    self.unconfirmed.remove(&peer);
                    if self.by_peer.contains_key(&peer) {
                        self.dormant.insert(peer);
                    }
                }
                ServerEvent::PeerResumed { peer, .. } => {
                    // The old link's unacknowledged events are gone with it: resync.
                    if self.dormant.remove(&peer)
                        && let Some(member) = self.by_peer.get(&peer).copied()
                    {
                        self.core.resync(member);
                    }
                }
                ServerEvent::PeerLeft { peer, .. } => {
                    self.unconfirmed.remove(&peer);
                    self.dormant.remove(&peer);
                    self.proof_failures.remove(&peer);
                    self.peer_wire.remove(&peer);
                    if let Some(member) = self.by_peer.remove(&peer) {
                        self.by_member.remove(&member);
                        self.core.disconnect(member);
                    }
                }
                _ => {}
            }
        }
        // A dormant session is held for the endpoint's grace: kept alive, and its events (which
        // the resumed link gets as a resync) are not queued for a link that is gone.
        let dormant: Vec<MemberId> = self
            .dormant
            .iter()
            .filter_map(|p| self.by_peer.get(p).copied())
            .collect();
        for member in dormant {
            let _ = self.core.heartbeat(member, now_ms);
            drop(self.core.take_events(member));
        }
        // A connection that never says hello holds no session, but it holds a peer slot: it gets
        // the same idle timeout a session has.
        let idle = self.core.settings().idle_ms;
        let silent: Vec<PeerId> = self
            .unconfirmed
            .iter()
            .filter(|(_, since)| now_ms.saturating_sub(**since) >= idle)
            .map(|(peer, _)| *peer)
            .collect();
        for peer in silent {
            self.unconfirmed.remove(&peer);
            self.core.note_refused("hub_no_hello");
            self.server.kick(peer, DisconnectReason::Timeout);
        }
        self.core.flush();
        self.drain_all();
        let expired = self.core.expire(now_ms);
        for member in &expired {
            if let Some(peer) = self.by_member.remove(member) {
                self.by_peer.remove(&peer);
                self.dormant.remove(&peer);
                self.peer_wire.remove(&peer);
                self.server.kick(peer, DisconnectReason::Timeout);
            }
        }
        if !expired.is_empty() {
            // The members who saw them leave.
            self.core.flush();
            self.drain_all();
        }
        self.server.flush(now_ms);
    }

    fn drain_all(&mut self) {
        let routes: Vec<(MemberId, PeerId)> = self
            .by_member
            .iter()
            .filter(|(_, p)| !self.dormant.contains(p))
            .map(|(m, p)| (*m, *p))
            .collect();
        for (member, peer) in routes {
            self.drain(member, peer);
        }
    }

    /// Sends `member`'s queued events in order. What the link will not take yet (back-pressure, or
    /// the per-peer send rate) goes back to the front of the outbox for the next update, so the
    /// reliable stream never skips an event; an event the link refuses outright is counted
    /// `hub_send_failed`.
    fn drain(&mut self, member: MemberId, peer: PeerId) {
        let wire = self.peer_wire.get(&peer).copied().unwrap_or(1);
        let mut events = self.core.take_events(member).into_iter();
        while let Some(event) = events.next() {
            let event = rewrite_for_wire(wire, event);
            let message = HubMessage::Event(event);
            match self.server.send(peer, Channel::GAME_RPC, &message) {
                Ok(()) => {}
                Err(SendError::Backpressure(_) | SendError::RateLimited) => {
                    let HubMessage::Event(event) = message else {
                        return;
                    };
                    let mut rest = vec![event];
                    rest.extend(events.map(|event| rewrite_for_wire(wire, event)));
                    self.core.requeue(member, rest);
                    return;
                }
                Err(_) => self.core.note_refused("hub_send_failed"),
            }
        }
    }

    fn reply_refused(&mut self, peer: PeerId, error: &HubError) {
        let event = super::HubEvent::Refused {
            code: error.code().to_owned(),
        };
        let _ = self
            .server
            .send(peer, Channel::GAME_RPC, &HubMessage::Event(event));
    }

    /// Whether a `relay_only` ask would put a generation-2 tail on a wire-1 owner's stream.
    ///
    /// The sender's dialect is not enough: a wire-2 member can ask a wire-1 owner. The tail is
    /// the byte `put_offer` appends, and a v0.7.0 client ends the link on it. An unknown lobby,
    /// or a caller who is not a member, returns false so the core's own error stands. An owner
    /// with no live link counts as wire 1: the queued ask would go out on reconnect, and that
    /// peer has not announced wire 2.
    fn relay_tail_hits_wire_1_owner(&self, member: MemberId, lobby: LobbyId) -> bool {
        let Some(info) = self.core.lobby_info(lobby) else {
            return false;
        };
        if info.owner == member || !info.has_member(member) {
            return false;
        }
        let Some(owner_peer) = self.by_member.get(&info.owner) else {
            return true;
        };
        u32::from(self.peer_wire.get(owner_peer).copied().unwrap_or(1)) < wire::HUB_WIRE_VERSION
    }

    /// Counts one refused keyed Hello of `peer` (ADR-8734, ADR-8883); at [`PROOF_FAILURE_LIMIT`]
    /// the link is dropped and what it still has queued is not checked.
    fn note_proof_failure(&mut self, peer: PeerId) {
        let failures = self.proof_failures.get(&peer).copied().unwrap_or(0) + 1;
        self.proof_failures.insert(peer, failures);
        if failures >= PROOF_FAILURE_LIMIT {
            self.unconfirmed.remove(&peer);
            self.core.note_refused("hub_proof_dropped");
            self.server.kick(peer, DisconnectReason::ProtocolError);
        }
    }

    fn on_message(&mut self, peer: PeerId, message: HubMessage, now_ms: u64) {
        let HubMessage::Request(request) = message else {
            // A client never sends events.
            self.core.note_refused("hub_unexpected_event");
            return;
        };
        let Some(member) = self.by_peer.get(&peer).copied() else {
            if let HubRequest::Hello { handle, proof } = request {
                // The player proof is optional (ADR-8633): none gives a session with no player key.
                // One that is present must bind to this peer's sealed link: a proof made for another
                // link (recorded, relayed, or for another hub) is refused. The address and identity
                // gate runs before either connect, and a draining hub takes no new session.
                if self.draining {
                    self.core.note_refused(HubError::Draining.code());
                    self.reply_refused(peer, &HubError::Draining);
                    return;
                }
                let keyed = !proof.is_empty();
                let proven = if !keyed {
                    None
                } else {
                    // A link that has used up its refused proofs is being dropped: what is still
                    // queued from it is not checked (ADR-8734).
                    let failures = self.proof_failures.get(&peer).copied().unwrap_or(0);
                    if failures >= PROOF_FAILURE_LIMIT {
                        return;
                    }
                    let proven = self
                        .server
                        .peer_handshake_hash(peer)
                        .and_then(|hash| verify_player_proof(&self.identity, &hash, &proof).ok());
                    let Some(identity) = proven else {
                        self.core.note_refused(HubError::IdentityMismatch.code());
                        self.reply_refused(peer, &HubError::IdentityMismatch);
                        self.note_proof_failure(peer);
                        return;
                    };
                    Some(identity)
                };
                let addr = self.server.session().peer(peer).map(|row| row.addr.ip());
                if let Err(error) = self.limits.admit(addr, proven.as_ref(), now_ms) {
                    self.core.note_refused(error.code());
                    self.reply_refused(peer, &error);
                    return;
                }
                let connected = if let Some(identity) = proven {
                    self.core.connect_player(identity, &handle, now_ms)
                } else {
                    self.core.connect(&handle, now_ms)
                };
                match connected {
                    Ok(member) => {
                        self.unconfirmed.remove(&peer);
                        self.proof_failures.remove(&peer);
                        // A reconnect under the same key takes the session over from its old link.
                        if let Some(old) = self.by_member.insert(member, peer)
                            && old != peer
                        {
                            self.by_peer.remove(&old);
                            self.dormant.remove(&old);
                            self.peer_wire.remove(&old);
                            self.server.kick(old, DisconnectReason::Kicked);
                        }
                        self.by_peer.insert(peer, member);
                        self.core.note_source(member, addr);
                        if let Some(hash) = self.server.peer_handshake_hash(peer) {
                            self.core.note_link_hash(member, hash);
                        }
                    }
                    Err(e) => {
                        self.reply_refused(peer, &e);
                        // A proof that verified cost the same key work as one that failed: a
                        // Hello refused after it (a bad or taken handle, the key in use, the
                        // session cap) counts toward the same limit, or one link could keep the
                        // hub verifying without bound (ADR-8883). A success clears the count.
                        if keyed {
                            self.note_proof_failure(peer);
                        }
                    }
                }
            } else {
                self.core.note_refused(HubError::NoSession.code());
                self.reply_refused(peer, &HubError::NoSession);
            }
            return;
        };
        let identity = self.core.identity(member);
        let addr = self.server.session().peer(peer).map(|row| row.addr.ip());
        if let Err(error) = self.limits.admit(addr, identity.as_ref(), now_ms) {
            self.core.note_refused(error.code());
            self.reply_refused(peer, &error);
            return;
        }
        self.core.note_source(member, addr);
        if let HubRequest::AnnounceWire { version } = request {
            if version == wire::HUB_WIRE_VERSION as u8 {
                self.peer_wire.insert(peer, version);
                self.core.set_wire(member, u32::from(version));
            } else {
                self.core.note_refused(HubError::NeedsWireV2.code());
                self.reply_refused(peer, &HubError::NeedsWireV2);
            }
            return;
        }
        let dialect = u32::from(self.peer_wire.get(&peer).copied().unwrap_or(1));
        if wire::gate_request(dialect, &request).is_err() {
            // A version-2 call from a wire-1 peer is a refusal, never a disconnect (ADR-10378).
            self.core.note_refused(HubError::NeedsWireV2.code());
            self.reply_refused(peer, &HubError::NeedsWireV2);
            return;
        }
        // Computed before `core` is borrowed. A wire-2 member's `relay_only` ask is legal for
        // the sender and still illegal for a wire-1 owner: `put_offer` would append the tail.
        let block_relay_tail = match &request {
            HubRequest::AskReach { lobby, offer } if offer.relay_only => {
                self.relay_tail_hits_wire_1_owner(member, *lobby)
            }
            _ => false,
        };
        let core = &mut self.core;
        let failed: Option<HubError> = match request {
            HubRequest::Hello { .. } | HubRequest::Heartbeat => core.heartbeat(member, now_ms).err(),
            HubRequest::List {
                request,
                dir_gen,
                presence_gen,
            } => {
                // The heartbeat first: the cursor's rewind window runs on the session's clock.
                let _ = core.heartbeat(member, now_ms);
                core.note_cursor(member, dir_gen, presence_gen);
                // Answer at once: the pages then the done marker, in order on the reliable stream.
                core.flush_member(member);
                core.queue(member, super::HubEvent::ListDone { request });
                self.drain(member, peer);
                None
            }
            HubRequest::SetPresence(p) => core.set_presence(member, p, now_ms).err(),
            HubRequest::SetFriends(list) => core.set_friends(member, &list, now_ms).err(),
            HubRequest::Create {
                request,
                visibility,
                max_members,
                joinable,
                data,
            } => {
                let spec = LobbySpec {
                    visibility,
                    max_members,
                    data,
                    joinable,
                };
                let result = core
                    .create(member, &spec, now_ms)
                    .map_err(|e| e.code().to_owned());
                self.send_reply(peer, request, result);
                None
            }
            HubRequest::Join {
                request,
                lobby,
                token,
            } => {
                let result = core
                    .join(member, lobby, token, now_ms)
                    .map(|()| lobby)
                    .map_err(|e| e.code().to_owned());
                self.send_reply(peer, request, result);
                None
            }
            HubRequest::Leave { lobby } => {
                core.leave(member, lobby, now_ms);
                None
            }
            HubRequest::SetData { lobby, key, value } => {
                core.set_data(member, lobby, &key, value.as_deref(), now_ms).err()
            }
            HubRequest::SetMemberData { lobby, key, value } => core
                .set_member_data(member, lobby, &key, value.as_deref(), now_ms)
                .err(),
            HubRequest::SetJoinable { lobby, joinable } => {
                core.set_joinable(member, lobby, joinable, now_ms).err()
            }
            HubRequest::SetServer { lobby, server } => {
                core.confirm_session(member, lobby, server, now_ms).err()
            }
            HubRequest::Invite { lobby, handle } => core.invite(member, lobby, &handle, now_ms).err(),
            HubRequest::Mute {
                lobby,
                member: target,
                muted,
            } => core.mute(member, lobby, target, muted, now_ms).err(),
            HubRequest::Chat { scope, text } => core.chat(member, scope, &text, now_ms).err(),
            HubRequest::Offer { lobby, offer } => {
                // Stored on the room. `Plan` encodes `relay_only: false`, so this arm does not
                // write a generation-2 tail to another peer. A wire-1 sender is already refused
                // by `gate_request` when the flag is set.
                core.offer_reach(member, lobby, offer, now_ms).err()
            }
            HubRequest::AskReach { lobby, offer } => {
                if block_relay_tail {
                    // Do not call `ask_reach`: it queues `ReachAsk` for the owner, and that
                    // event carries the tail. The member is refused; the owner is sent nothing.
                    core.note_refused(HubError::NeedsWireV2.code());
                    Some(HubError::NeedsWireV2)
                } else {
                    core.ask_reach(member, lobby, offer, now_ms).err()
                }
            }
            HubRequest::Approve {
                lobby,
                member: target,
                approve,
            } => core.approve(member, lobby, target, approve, now_ms).err(),
            HubRequest::ReachDone {
                lobby,
                path,
                punch_attempts,
            } => core.reach_done(member, lobby, path, punch_attempts, now_ms).err(),
            HubRequest::AnnounceWire { .. } => None,
            HubRequest::RegisterServer { presence } => {
                super::join::register_server(core, member, &presence, now_ms).err()
            }
            HubRequest::RevokeBinding { server_key, serial } => {
                super::join::revoke_binding(core, member, server_key, serial, now_ms).err()
            }
            HubRequest::CreateDelegated {
                request,
                visibility,
                max_members,
                joinable,
                data,
                server_key,
            } => {
                let spec = LobbySpec {
                    visibility,
                    max_members,
                    data,
                    joinable,
                };
                let result = super::join::create_delegated(core, member, &spec, server_key, now_ms)
                    .map_err(|e| e.code().to_owned());
                self.send_reply(peer, request, result);
                None
            }
            HubRequest::SetReachMode { lobby, relay_only } => {
                super::join::set_reach_mode(core, member, lobby, relay_only, now_ms).err()
            }
            HubRequest::SetBans { lobby, keys } => core.set_bans(member, lobby, &keys, now_ms).err(),
            HubRequest::CloseLobby { lobby } => super::join::close_lobby(core, member, lobby, now_ms).err(),
        };
        if let Some(e) = failed {
            self.reply_refused(peer, &e);
        }
        let replaced = self.core.take_replaced_servers();
        for old in replaced {
            self.core.note_refused(HubError::ServerInUse.code());
            let Some(old_peer) = self.by_member.remove(&old) else {
                continue;
            };
            self.by_peer.remove(&old_peer);
            self.dormant.remove(&old_peer);
            self.peer_wire.remove(&old_peer);
            self.reply_refused(old_peer, &HubError::ServerInUse);
            self.core.disconnect(old);
            self.server.kick(old_peer, DisconnectReason::Kicked);
        }
    }

    fn send_reply(&mut self, peer: PeerId, request: u64, result: Result<crate::lobby::LobbyId, String>) {
        // The reply goes after the events the operation queued (the room first, then the reply), so
        // the client's view is whole when it reports the result.
        let reply = super::HubEvent::Reply { request, result };
        if let Some(member) = self.by_peer.get(&peer).copied() {
            self.core.queue(member, reply);
            self.drain(member, peer);
        } else {
            let _ = self
                .server
                .send(peer, Channel::GAME_RPC, &HubMessage::Event(reply));
        }
    }
}

/// A wire-1 peer still decodes [`HubEvent::ServerSet`]. `SessionConfirmed` has no address, and
/// neither does the `ServerSet` of `None` a version-1 peer is sent instead. Wire 2 and newer
/// receive `SessionConfirmed`.
fn rewrite_for_wire(wire: u8, event: HubEvent) -> HubEvent {
    if wire >= 2 {
        return event;
    }
    if let HubEvent::SessionConfirmed { lobby, .. } = event {
        HubEvent::ServerSet { lobby, server: None }
    } else {
        event
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::{HubHost, PROOF_FAILURE_LIMIT};
    use crate::client::{ClientConfig, ClientEvent, NetClient};
    use crate::connection::Channel;
    use crate::crypto::{
        CryptoMode, PlayerIdentity, PlayerKey, ServerIdentity, ServerKey, forge_zero_key_proof,
    };
    use crate::lobby::hub::{HubEvent, HubMessage, HubRequest, HubSettings, HubWire};
    use crate::lobby::{LobbySpec, LobbyVisibility};
    use crate::socket::{LoopbackNet, LoopbackSocket};

    fn hub_addr() -> SocketAddr {
        SocketAddr::from(([192, 0, 2, 9], 7810))
    }

    fn open_hub(secret: u8) -> (LoopbackNet, ServerKey, HubHost<LoopbackSocket>) {
        let net = LoopbackNet::new();
        let identity = ServerIdentity::from_private([secret; 32]).expect("curve");
        let key = identity.public();
        let hub = HubHost::new(
            net.bind(hub_addr()).expect("bind hub"),
            identity,
            HubSettings::default(),
        );
        (net, key, hub)
    }

    fn connect_raw(
        net: &LoopbackNet,
        hub: &mut HubHost<LoopbackSocket>,
        port: u16,
        key: ServerKey,
        now: &mut u64,
    ) -> NetClient<HubWire, LoopbackSocket> {
        let mut raw = NetClient::connect(
            net.bind(SocketAddr::from(([192, 0, 2, 60], port))).expect("bind"),
            hub_addr(),
            ClientConfig {
                crypto: CryptoMode::Required,
                server_key: Some(key),
                ..ClientConfig::default()
            },
            &(),
        )
        .expect("connect fits");
        while !raw.is_connected() {
            *now += 10;
            raw.update(*now);
            hub.update(*now);
            assert!(*now < 5_000, "the handshake did not finish");
        }
        raw
    }

    fn pump(
        hub: &mut HubHost<LoopbackSocket>,
        raw: &mut NetClient<HubWire, LoopbackSocket>,
        now: &mut u64,
        steps: u32,
    ) -> Vec<String> {
        let mut refused = Vec::new();
        for _ in 0..steps {
            *now += 10;
            raw.update(*now);
            hub.update(*now);
            while let Some(event) = raw.poll_event() {
                if let ClientEvent::Message {
                    message: HubMessage::Event(HubEvent::Refused { code }),
                    ..
                } = event
                {
                    refused.push(code);
                }
            }
        }
        refused
    }

    fn hello(handle: &str, proof: Vec<u8>) -> HubMessage {
        HubMessage::Request(HubRequest::Hello {
            handle: handle.into(),
            proof,
        })
    }

    /// A live session stays one session when its proof is sent again, and a second link cannot
    /// take it with that proof, a truncated proof, or an all-zero buffer.
    #[test]
    fn a_proof_cannot_be_replayed_or_kick_without_the_key() {
        let (net, key, mut hub) = open_hub(0x21);
        let mut now = 0;
        let ann = PlayerIdentity::from_private([5; 32]).expect("curve");
        let mut raw = connect_raw(&net, &mut hub, 6200, key, &mut now);
        let binding = raw.handshake_hash().expect("sealed");
        let proof = ann.prove(&key, &binding).expect("proof");
        raw.send(Channel::GAME_RPC, &hello("ann", proof.clone()))
            .expect("send");
        let refused = pump(&mut hub, &mut raw, &mut now, 40);
        assert!(refused.is_empty(), "the real proof is accepted, got {refused:?}");
        let member = hub.core().member_by_handle("ann").expect("a session");
        assert_eq!(hub.core().identity(member), Some(ann.public()));
        // The same proof on the same link is a heartbeat, not a second session.
        raw.send(Channel::GAME_RPC, &hello("ann", proof.clone()))
            .expect("send again");
        let refused = pump(&mut hub, &mut raw, &mut now, 40);
        assert!(
            refused.is_empty(),
            "a second hello is a heartbeat, got {refused:?}"
        );
        assert_eq!(hub.core().players().len(), 1);
        assert_eq!(hub.core().member_by_handle("ann"), Some(member));

        let mut other = connect_raw(&net, &mut hub, 6201, key, &mut now);
        other
            .send(Channel::GAME_RPC, &hello("ann", proof))
            .expect("replay");
        let refused = pump(&mut hub, &mut other, &mut now, 40);
        assert_eq!(refused, ["hub_identity_mismatch".to_owned()]);
        assert_eq!(
            hub.core().member_by_handle("ann"),
            Some(member),
            "a replay on another link does not kick the live session"
        );

        let mut truncated = connect_raw(&net, &mut hub, 6202, key, &mut now);
        truncated
            .send(Channel::GAME_RPC, &hello("ann", vec![0; 10]))
            .expect("short proof");
        truncated
            .send(Channel::GAME_RPC, &hello("eve", vec![0; 128]))
            .expect("zero buffer");
        let refused = pump(&mut hub, &mut truncated, &mut now, 40);
        assert_eq!(
            refused,
            [
                "hub_identity_mismatch".to_owned(),
                "hub_identity_mismatch".to_owned()
            ]
        );
        assert_eq!(hub.core().players().len(), 1);
        assert_eq!(hub.core().identity(member), Some(ann.public()));
    }

    /// After the link drops, the proof from that handshake is refused on the next one.
    #[test]
    fn a_proof_from_the_previous_session_is_refused_after_reconnect() {
        let (net, key, mut hub) = open_hub(0x22);
        let mut now = 0;
        let ann = PlayerIdentity::from_private([6; 32]).expect("curve");
        let mut raw = connect_raw(&net, &mut hub, 6300, key, &mut now);
        let old_binding = raw.handshake_hash().expect("sealed");
        let old_proof = ann.prove(&key, &old_binding).expect("proof");
        raw.send(Channel::GAME_RPC, &hello("ann", old_proof.clone()))
            .expect("send");
        let refused = pump(&mut hub, &mut raw, &mut now, 40);
        assert!(refused.is_empty(), "{refused:?}");
        assert!(hub.core().member_by_handle("ann").is_some());
        raw.disconnect();
        for _ in 0..40 {
            now += 10;
            hub.update(now);
        }
        assert!(hub.core().players().is_empty(), "the goodbye drops the session");

        let mut again = connect_raw(&net, &mut hub, 6301, key, &mut now);
        let new_binding = again.handshake_hash().expect("sealed");
        assert_ne!(new_binding, old_binding, "a new link has a new handshake hash");
        again
            .send(Channel::GAME_RPC, &hello("ann", old_proof))
            .expect("old proof");
        let refused = pump(&mut hub, &mut again, &mut now, 40);
        assert_eq!(refused, ["hub_identity_mismatch".to_owned()]);
        assert!(hub.core().players().is_empty(), "the old proof opens nothing");
        let fresh = ann.prove(&key, &new_binding).expect("new proof");
        again
            .send(Channel::GAME_RPC, &hello("ann", fresh))
            .expect("new proof");
        let refused = pump(&mut hub, &mut again, &mut now, 40);
        assert!(refused.is_empty(), "{refused:?}");
        let member = hub.core().member_by_handle("ann").expect("reconnected");
        assert_eq!(hub.core().identity(member), Some(ann.public()));
    }

    /// A zero-DH proof must not open a session, and must not get into a friends-only lobby even
    /// when the owner's friend list holds the zero key (a keyless session's `Welcome` shows it).
    /// Asserts the safe property: it fails if the hub accepts `PlayerKey([0; 32])`. Friends are
    /// player keys (ADR-8633); the real friend, listed by key, still joins.
    #[test]
    fn a_zero_key_proof_does_not_open_a_session_or_pin_a_friend() {
        let (net, key, mut hub) = open_hub(0x23);
        let mut now = 0;
        let owner = PlayerIdentity::from_private([1; 32]).expect("curve");
        let sam = PlayerIdentity::from_private([2; 32]).expect("curve");
        let mut host = connect_raw(&net, &mut hub, 6400, key, &mut now);
        let host_binding = host.handshake_hash().expect("sealed");
        host.send(
            Channel::GAME_RPC,
            &hello("host", owner.prove(&key, &host_binding).expect("proof")),
        )
        .expect("send");
        let refused = pump(&mut hub, &mut host, &mut now, 40);
        assert!(refused.is_empty(), "{refused:?}");
        let owner_id = hub.core().member_by_handle("host").expect("owner");
        hub.core_mut()
            .set_friends(owner_id, &[sam.public(), PlayerKey::default()], now)
            .expect("friends");
        let spec = LobbySpec {
            visibility: LobbyVisibility::FriendsOnly,
            ..LobbySpec::default()
        };
        let lobby = hub.core_mut().create(owner_id, &spec, now).expect("lobby");

        let mut attacker = connect_raw(&net, &mut hub, 6401, key, &mut now);
        let binding = attacker.handshake_hash().expect("sealed");
        let forged = forge_zero_key_proof(&key, &binding).expect("zero-key builder");
        assert_eq!(forged.len(), crate::crypto::PLAYER_PROOF_BYTES);
        attacker
            .send(Channel::GAME_RPC, &hello("sam", forged))
            .expect("forged hello");
        let refused = pump(&mut hub, &mut attacker, &mut now, 40);
        let opened = hub.core().member_by_handle("sam");
        let pin = opened.map(|id| {
            let joined = hub.core_mut().join(id, lobby, None, now);
            format!("identity {:?}, join {joined:?}", hub.core().identity(id))
        });
        assert!(
            opened.is_none(),
            "a zero-key hello was refused as {refused:?} but opened a session ({pin:?})"
        );
        assert_eq!(refused, ["hub_identity_mismatch".to_owned()]);

        let mut real = connect_raw(&net, &mut hub, 6402, key, &mut now);
        let real_binding = real.handshake_hash().expect("sealed");
        real.send(
            Channel::GAME_RPC,
            &hello("sam", sam.prove(&key, &real_binding).expect("proof")),
        )
        .expect("sam");
        let refused = pump(&mut hub, &mut real, &mut now, 40);
        assert!(refused.is_empty(), "{refused:?}");
        let sam_id = hub.core().member_by_handle("sam").expect("the real friend");
        assert_eq!(hub.core().identity(sam_id), Some(sam.public()));
        hub.core_mut()
            .join(sam_id, lobby, None, now)
            .expect("the listed friend is not pinned to a key nobody holds");
        assert!(
            hub.core()
                .players()
                .iter()
                .all(|p| hub.core().identity(p.member) != Some(PlayerKey::default())),
            "no session holds the zero key"
        );
    }

    /// v06-security-fixes-verification-c: a link that keeps sending failing player proofs costs
    /// the hub a proof check (two X25519 in `read_message`, two more in the ADR-8665 low-order
    /// check) per `Hello`, with nothing to stop it: a refused `Hello` leaves the peer connected and
    /// unconfirmed until the idle timeout. An honest client sends one proof per link, and a
    /// refused one does not start to verify on a retry. Asserts the safe property: one link gets
    /// a bounded number of proof checks (here 8 in one second), then the hub stops checking or drops the
    /// link. Fails while every forged `Hello` is verified.
    #[test]
    fn a_link_that_keeps_failing_its_proof_is_not_verified_without_bound() {
        let (net, key, mut hub) = open_hub(0x24);
        let mut now = 0;
        let mut attacker = connect_raw(&net, &mut hub, 6500, key, &mut now);
        let binding = attacker.handshake_hash().expect("sealed");
        let forged = forge_zero_key_proof(&key, &binding).expect("zero-key builder");
        let checked = |hub: &HubHost<LoopbackSocket>| {
            hub.core()
                .report()
                .refused
                .get("hub_identity_mismatch")
                .copied()
                .unwrap_or(0)
        };
        let before = checked(&hub);
        let mut sent = 0u32;
        // One second of virtual time, four forged hellos every 10 ms: 400 in all, well under the
        // endpoint's default inbound message rate.
        for _ in 0..100 {
            if attacker.is_connected() {
                for _ in 0..4 {
                    if attacker
                        .send(Channel::GAME_RPC, &hello("mallory", forged.clone()))
                        .is_ok()
                    {
                        sent += 1;
                    }
                }
            }
            let _ = pump(&mut hub, &mut attacker, &mut now, 1);
        }
        let verified = checked(&hub).saturating_sub(before);
        assert!(sent >= 8, "the attacker got {sent} hellos onto its link");
        assert!(
            hub.core().member_by_handle("mallory").is_none(),
            "no forged hello opens a session"
        );
        assert!(
            verified <= 8,
            "one link had {verified} failing proofs checked in one second ({sent} sent, still connected: {}); \
             each costs four X25519",
            attacker.is_connected()
        );
    }

    /// ADR-8734: the link is dropped at the fourth refused proof (counted `hub_proof_dropped`),
    /// what it still had queued is not checked, and the same player then proves on a new link.
    /// Three refused proofs (a zero-key one, one for another link, a zero buffer) leave it up.
    #[test]
    fn the_fourth_refused_proof_drops_the_link_and_a_new_link_still_proves() {
        let (net, key, mut hub) = open_hub(0x25);
        let mut now = 0;
        let ann = PlayerIdentity::from_private([9; 32]).expect("curve");
        let count = |hub: &HubHost<LoopbackSocket>, code: &str| {
            hub.core().report().refused.get(code).copied().unwrap_or(0)
        };
        let mut raw = connect_raw(&net, &mut hub, 6600, key, &mut now);
        let binding = raw.handshake_hash().expect("sealed");
        let forged = forge_zero_key_proof(&key, &binding).expect("zero-key builder");
        let wrong_link = ann.prove(&key, &[0x55; 32]).expect("proof");
        for proof in [
            forged.clone(),
            wrong_link,
            vec![0; crate::crypto::PLAYER_PROOF_BYTES],
        ] {
            raw.send(Channel::GAME_RPC, &hello("ann", proof)).expect("send");
        }
        let refused = pump(&mut hub, &mut raw, &mut now, 20);
        assert_eq!(refused.len(), 3, "{refused:?}");
        assert!(raw.is_connected(), "three refused proofs keep the link");
        assert_eq!(count(&hub, "hub_proof_dropped"), 0);
        // The fourth, and two more queued behind it in the same receive.
        for _ in 0..3 {
            raw.send(Channel::GAME_RPC, &hello("ann", forged.clone()))
                .expect("send");
        }
        let _ = pump(&mut hub, &mut raw, &mut now, 40);
        assert_eq!(
            count(&hub, "hub_identity_mismatch"),
            u64::from(PROOF_FAILURE_LIMIT),
            "proofs queued behind the limit are not checked"
        );
        assert_eq!(count(&hub, "hub_proof_dropped"), 1);
        assert!(!raw.is_connected(), "the hub dropped the link");
        assert!(hub.core().players().is_empty());

        let mut again = connect_raw(&net, &mut hub, 6601, key, &mut now);
        let fresh = ann
            .prove(&key, &again.handshake_hash().expect("sealed"))
            .expect("proof");
        again.send(Channel::GAME_RPC, &hello("ann", fresh)).expect("send");
        let refused = pump(&mut hub, &mut again, &mut now, 40);
        assert!(refused.is_empty(), "{refused:?}");
        let member = hub.core().member_by_handle("ann").expect("a session");
        assert_eq!(hub.core().identity(member), Some(ann.public()));
    }

    /// Next to ADR-8734's proof cap: the counter moves only when `verify_player_proof` fails.
    /// A proof that verifies (four X25519: the Noise read, then the low-order check) and whose
    /// hello is then refused — an empty handle, `hub_handle_invalid`, which is reached only after
    /// the proof verifies — is not counted and does not drop the link. Same load as
    /// `a_link_that_keeps_failing_its_proof_is_not_verified_without_bound`: four hellos every
    /// 10 ms for one virtual second. Asserts the same bound, [`PROOF_FAILURE_LIMIT`] checks,
    /// then the hub stops or drops the link.
    #[test]
    fn a_verified_proof_with_a_refused_hello_is_still_checked_without_bound() {
        let (net, key, mut hub) = open_hub(0x26);
        let mut now = 0;
        let ann = PlayerIdentity::from_private([11; 32]).expect("curve");
        let mut raw = connect_raw(&net, &mut hub, 6700, key, &mut now);
        let binding = raw.handshake_hash().expect("sealed");
        let proof = ann.prove(&key, &binding).expect("proof");
        let count = |hub: &HubHost<LoopbackSocket>, code: &str| {
            hub.core().report().refused.get(code).copied().unwrap_or(0)
        };
        let mut sent = 0u32;
        // One second of virtual time, four hellos every 10 ms.
        for _ in 0..100 {
            if raw.is_connected() {
                for _ in 0..4 {
                    if raw.send(Channel::GAME_RPC, &hello("", proof.clone())).is_ok() {
                        sent += 1;
                    }
                }
            }
            let _ = pump(&mut hub, &mut raw, &mut now, 1);
        }
        let checked = count(&hub, "hub_handle_invalid");
        assert!(sent >= 8, "the attacker got {sent} hellos onto its link");
        assert!(
            hub.core().players().is_empty(),
            "an empty handle opens no session"
        );
        assert_eq!(
            count(&hub, "hub_identity_mismatch"),
            0,
            "the proof verified; the refusal is the handle"
        );
        assert!(
            checked <= u64::from(PROOF_FAILURE_LIMIT),
            "one link had {checked} verified-then-refused hellos checked in one second \
             ({sent} sent, still connected: {}); each costs four X25519 and is not counted \
             toward PROOF_FAILURE_LIMIT (hub_proof_dropped {})",
            raw.is_connected(),
            count(&hub, "hub_proof_dropped")
        );
    }

    fn register_server() -> HubMessage {
        HubMessage::Request(HubRequest::RegisterServer {
            presence: crate::lobby::hub::ServerPresence {
                server_proof: vec![0x11; crate::crypto::PLAYER_PROOF_BYTES],
                proofs: Vec::new(),
                capacity: 1,
                load: 0,
                protocol: 1,
                game_version: "2".into(),
                relay_only: false,
                grace_ms: 0,
                bans: Vec::new(),
            },
        })
    }

    /// A v0.7.0 peer (no `AnnounceWire`) stays on wire 1. A version-2 call is
    /// `hub_needs_wire_v2` and the link stays up. After generation 2 is announced a dummy
    /// presence proof is `hub_binding_invalid` and a heartbeat still works.
    #[test]
    fn a_wire_1_peer_keeps_its_session_when_a_v2_call_is_refused() {
        let (net, key, mut hub) = open_hub(0x27);
        let mut now = 0;
        let ann = PlayerIdentity::from_private([12; 32]).expect("curve");
        let mut raw = connect_raw(&net, &mut hub, 6800, key, &mut now);
        let proof = ann
            .prove(&key, &raw.handshake_hash().expect("sealed"))
            .expect("proof");
        raw.send(Channel::GAME_RPC, &hello("ann", proof)).expect("hello");
        let refused = pump(&mut hub, &mut raw, &mut now, 40);
        assert!(refused.is_empty(), "{refused:?}");
        assert!(hub.core().member_by_handle("ann").is_some());

        raw.send(Channel::GAME_RPC, &register_server()).expect("register");
        let refused = pump(&mut hub, &mut raw, &mut now, 40);
        assert_eq!(refused, ["hub_needs_wire_v2".to_owned()]);
        assert!(raw.is_connected(), "a v2 call does not drop a v1 peer");
        assert!(hub.core().member_by_handle("ann").is_some());
        assert_eq!(hub.report().refused.get("hub_needs_wire_v2").copied(), Some(1));

        raw.send(Channel::GAME_RPC, &HubMessage::Request(HubRequest::Heartbeat))
            .expect("heartbeat");
        let refused = pump(&mut hub, &mut raw, &mut now, 20);
        assert!(refused.is_empty(), "a v1 heartbeat still works, got {refused:?}");
        assert!(raw.is_connected());

        raw.send(
            Channel::GAME_RPC,
            &HubMessage::Request(HubRequest::AnnounceWire { version: 1 }),
        )
        .expect("bad announce");
        let refused = pump(&mut hub, &mut raw, &mut now, 20);
        assert_eq!(refused, ["hub_needs_wire_v2".to_owned()]);
        assert!(raw.is_connected());

        raw.send(Channel::GAME_RPC, &register_server()).expect("still v1");
        let refused = pump(&mut hub, &mut raw, &mut now, 20);
        assert_eq!(refused, ["hub_needs_wire_v2".to_owned()]);

        raw.send(
            Channel::GAME_RPC,
            &HubMessage::Request(HubRequest::AnnounceWire { version: 2 }),
        )
        .expect("announce");
        let refused = pump(&mut hub, &mut raw, &mut now, 20);
        assert!(refused.is_empty(), "{refused:?}");
        raw.send(Channel::GAME_RPC, &register_server())
            .expect("register v2");
        let refused = pump(&mut hub, &mut raw, &mut now, 40);
        assert_eq!(refused, ["hub_binding_invalid".to_owned()]);
        assert!(raw.is_connected(), "a bad binding does not drop the peer");
        assert!(hub.core().member_by_handle("ann").is_some());
        assert_eq!(hub.report().refused.get("hub_binding_invalid").copied(), Some(1));

        raw.send(Channel::GAME_RPC, &HubMessage::Request(HubRequest::Heartbeat))
            .expect("heartbeat after");
        let refused = pump(&mut hub, &mut raw, &mut now, 20);
        assert!(refused.is_empty(), "{refused:?}");
        assert!(raw.is_connected());
        assert_eq!(hub.server().session().peers().count(), 1);
    }

    /// A sealed server link registers with a real presence proof, and the bound owner creates.
    #[test]
    fn a_real_presence_registers_and_the_owner_creates() {
        let (net, key, mut hub) = open_hub(0x31);
        let mut now = 0u64;
        let ann = PlayerIdentity::from_private([12; 32]).expect("curve");
        let game = ServerIdentity::from_private([0x71; 32]).expect("curve");
        let mut owner = connect_raw(&net, &mut hub, 6900, key, &mut now);
        let owner_proof = ann
            .prove(&key, &owner.handshake_hash().expect("sealed"))
            .expect("proof");
        owner
            .send(Channel::GAME_RPC, &hello("ann", owner_proof))
            .expect("hello");
        assert!(pump(&mut hub, &mut owner, &mut now, 40).is_empty());

        let mut srv = connect_raw(&net, &mut hub, 6901, key, &mut now);
        srv.send(Channel::GAME_RPC, &hello("srv", Vec::new()))
            .expect("hello");
        assert!(pump(&mut hub, &mut srv, &mut now, 40).is_empty());
        srv.send(
            Channel::GAME_RPC,
            &HubMessage::Request(HubRequest::AnnounceWire { version: 2 }),
        )
        .expect("announce");
        assert!(pump(&mut hub, &mut srv, &mut now, 20).is_empty());

        let claims = crate::lobby::hub::BindingClaims {
            version: 1,
            server_key: game.public(),
            expiry_ms: 0,
            serial: 3,
        };
        let payload = crate::lobby::hub::encode_binding_payload(&claims).expect("payload");
        let binding = crate::crypto::binding::prove_binding(&ann, &key, &payload).expect("binding");
        let presence = crate::crypto::binding::prove_server_presence(
            &game,
            &key,
            &srv.handshake_hash().expect("sealed"),
        )
        .expect("presence");
        srv.send(
            Channel::GAME_RPC,
            &HubMessage::Request(HubRequest::RegisterServer {
                presence: crate::lobby::hub::ServerPresence {
                    server_proof: presence,
                    proofs: vec![binding],
                    capacity: 4,
                    load: 0,
                    protocol: 1,
                    game_version: "2".into(),
                    relay_only: false,
                    grace_ms: 0,
                    bans: Vec::new(),
                },
            }),
        )
        .expect("register");
        let refused = pump(&mut hub, &mut srv, &mut now, 40);
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(hub.report().servers.len(), 1);
        assert!(srv.is_connected());

        owner
            .send(
                Channel::GAME_RPC,
                &HubMessage::Request(HubRequest::AnnounceWire { version: 2 }),
            )
            .expect("announce");
        assert!(pump(&mut hub, &mut owner, &mut now, 20).is_empty());
        owner
            .send(
                Channel::GAME_RPC,
                &HubMessage::Request(HubRequest::CreateDelegated {
                    request: 1,
                    visibility: LobbyVisibility::Public,
                    max_members: 8,
                    joinable: true,
                    data: Default::default(),
                    server_key: game.public(),
                }),
            )
            .expect("create");
        let refused = pump(&mut hub, &mut owner, &mut now, 40);
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(hub.core().lobbies().len(), 1);
        assert_eq!(
            hub.core().lobbies()[0].kind,
            crate::lobby::hub::LobbyKind::Dedicated
        );
        assert!(owner.is_connected());
        assert!(srv.is_connected());
    }

    fn pump_two(
        hub: &mut HubHost<LoopbackSocket>,
        owner: &mut NetClient<HubWire, LoopbackSocket>,
        member: &mut NetClient<HubWire, LoopbackSocket>,
        now: &mut u64,
        steps: u32,
    ) -> (Vec<String>, Vec<HubEvent>) {
        let mut member_refused = Vec::new();
        let mut owner_events = Vec::new();
        for _ in 0..steps {
            *now += 10;
            owner.update(*now);
            member.update(*now);
            hub.update(*now);
            while let Some(event) = owner.poll_event() {
                if let ClientEvent::Message {
                    message: HubMessage::Event(ev),
                    ..
                } = event
                {
                    owner_events.push(ev);
                }
            }
            while let Some(event) = member.poll_event() {
                if let ClientEvent::Message {
                    message: HubMessage::Event(HubEvent::Refused { code }),
                    ..
                } = event
                {
                    member_refused.push(code);
                }
            }
        }
        (member_refused, owner_events)
    }

    /// A `relay_only` ask is a version-2 tail on a message a v0.7.0 peer also uses.
    ///
    /// The owner has not announced wire 2. The hub must refuse the member with
    /// `hub_needs_wire_v2` and send the owner no `ReachAsk`. The tail is one byte past the
    /// version-1 offer. A v0.7.0 client reads hub traffic on the reliable channel and ends the
    /// link on trailing bytes (`DisconnectReason::ProtocolError`). Both sessions stay up.
    #[test]
    fn a_relay_only_ask_is_not_forwarded_to_a_wire_1_owner() {
        let (net, key, mut hub) = open_hub(0x31);
        let mut now = 0;
        let owner_id = PlayerIdentity::from_private([12; 32]).expect("curve");
        let member_id = PlayerIdentity::from_private([13; 32]).expect("curve");
        let mut owner = connect_raw(&net, &mut hub, 6910, key, &mut now);
        let mut member = connect_raw(&net, &mut hub, 6911, key, &mut now);
        let owner_proof = owner_id
            .prove(&key, &owner.handshake_hash().expect("sealed"))
            .expect("proof");
        let member_proof = member_id
            .prove(&key, &member.handshake_hash().expect("sealed"))
            .expect("proof");
        owner
            .send(Channel::GAME_RPC, &hello("owner", owner_proof))
            .expect("hello");
        member
            .send(Channel::GAME_RPC, &hello("member", member_proof))
            .expect("hello");
        let _ = pump_two(&mut hub, &mut owner, &mut member, &mut now, 40);
        let owner_member = hub.core().member_by_handle("owner").expect("owner session");
        let member_member = hub.core().member_by_handle("member").expect("member session");
        let lobby = hub
            .core_mut()
            .create(owner_member, &LobbySpec::default(), now)
            .expect("lobby");
        hub.core_mut()
            .join(member_member, lobby, None, now)
            .expect("join");

        let mut offer = crate::lobby::hub::ReachOffer {
            candidates: vec![crate::nat::Candidate::new(
                crate::nat::CandidateKind::Host,
                SocketAddr::from(([10, 0, 0, 2], 7777)),
            )],
            ..crate::lobby::hub::ReachOffer::default()
        };
        owner
            .send(
                Channel::GAME_RPC,
                &HubMessage::Request(HubRequest::Offer {
                    lobby,
                    offer: offer.clone(),
                }),
            )
            .expect("offer");
        let refused_before = hub.report().refused.clone();
        let _ = pump_two(&mut hub, &mut owner, &mut member, &mut now, 20);
        assert_eq!(
            hub.report().refused,
            refused_before,
            "the version-1 offer is accepted"
        );

        member
            .send(
                Channel::GAME_RPC,
                &HubMessage::Request(HubRequest::AnnounceWire { version: 2 }),
            )
            .expect("announce");
        let (refused, _) = pump_two(&mut hub, &mut owner, &mut member, &mut now, 20);
        assert!(
            refused.is_empty(),
            "announcing wire 2 is accepted, got {refused:?}"
        );

        offer.relay_only = true;
        member
            .send(
                Channel::GAME_RPC,
                &HubMessage::Request(HubRequest::AskReach { lobby, offer }),
            )
            .expect("ask");
        let (member_refused, owner_events) = pump_two(&mut hub, &mut owner, &mut member, &mut now, 40);
        assert!(owner.is_connected(), "the wire-1 owner stays connected");
        assert!(member.is_connected(), "the wire-2 member stays connected");
        assert!(hub.core().member_by_handle("owner").is_some());
        assert!(hub.core().member_by_handle("member").is_some());
        let asks: Vec<_> = owner_events
            .iter()
            .filter(|ev| matches!(ev, HubEvent::ReachAsk { .. }))
            .collect();
        assert!(
            asks.is_empty() && member_refused == ["hub_needs_wire_v2".to_owned()],
            "a relay-only ask toward a wire-1 owner is hub_needs_wire_v2 and is not forwarded; \
             owner asks {asks:?}; member refused {member_refused:?}"
        );
    }
}
