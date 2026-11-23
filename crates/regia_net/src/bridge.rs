// SPDX-License-Identifier: MPL-2.0
//! Carries the replication runtime's frames (`crate::runtime::NetNode`) over the UDP endpoints
//! `NetServer` / `NetClient` (ADR-1643).
//!
//! A game that lets `regia_net` own its message stream names [`NetFrame`] as its
//! `NetGame::Message`. Each frame of the node is cut into parts of at most [`FRAME_PART_BYTES`]
//! and reassembled on the other side (the reliable channel delivers parts in order); an unreliable
//! state frame larger than one datagram is omitted; state producers split their records to fit.
//! Oversized game-message batches retain the reliable fallback. Per frame of the app:
//!
//! ```text
//! server.receive(now); bridge.pump_in(world, &mut server); app.update(); bridge.pump_out(world, &mut server); server.flush(now);
//! ```
//!
//! Joins and leaves of the endpoint become `server_add_peer` / `server_remove_peer`; disconnects the
//! node decides (flood, invalid messages, reliable overflow) become `NetServer::kick`.

use std::collections::{HashMap, VecDeque};

use regia_ecs::prelude::World;

use crate::client::{ClientEnd, ClientEvent, NetClient};
use crate::connection::{Channel as Wire, SendError};
use crate::handshake::DisconnectReason;
use crate::message::Channel;
use crate::model::{LocalPeer, PeerId};
use crate::payload::{NetCodec, NetGame};
use crate::runtime::{
    Datagram, MAX_KEPT_REJECTIONS, NetNode, NetRejection, ensure_resources, server_add_peer,
    server_remove_peer,
};
use crate::server::{NetServer, ServerEvent};
use crate::socket::DatagramSocket;
use crate::wire::{WireError, WireReader, WireWriter};

/// Largest part of a frame on the wire.
pub const FRAME_PART_BYTES: usize = 16 * 1024;
/// Largest frame reassembled from parts; a peer that sends more is disconnected.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Largest frame a client may send. A client frame is one tick of inputs and messages: far past
/// the flood limits long before this.
pub const MAX_CLIENT_FRAME_BYTES: usize = 1024 * 1024;
/// Bytes waiting for the endpoint per connection. A peer that stops acknowledging cannot grow the
/// server past this: it is disconnected with `ReliableOverflow`.
pub const MAX_BACKLOG_BYTES: usize = 2 * MAX_FRAME_BYTES;

/// One part of a replication frame: the game message type of a game whose stream `regia_net`
/// owns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetFrame {
    /// The last part of its frame.
    pub last: bool,
    /// The part's bytes (at most [`FRAME_PART_BYTES`]).
    pub bytes: Vec<u8>,
}

impl NetCodec for NetFrame {
    fn encode(&self, w: &mut WireWriter) {
        w.u8(u8::from(self.last));
        // Parts are cut at `FRAME_PART_BYTES`. A hand-made longer part fails the encode
        // instead of writing an empty field.
        if let Err(err) = w.bytes_u16(&self.bytes, FRAME_PART_BYTES) {
            w.fail(err);
        }
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        let last = match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(WireError::Invalid("frame part flag")),
        };
        Ok(Self {
            last,
            bytes: r.bytes_u16(FRAME_PART_BYTES)?.to_vec(),
        })
    }
}

/// Splits `bytes` into frame parts of at most [`FRAME_PART_BYTES`]. The first part keeps
/// `bytes`' allocation; each `split_off` allocates only the tail. An empty buffer is one empty
/// last part.
fn parts(mut bytes: Vec<u8>) -> Vec<NetFrame> {
    if bytes.len() <= FRAME_PART_BYTES {
        return vec![NetFrame { last: true, bytes }];
    }
    let mut out = Vec::new();
    while bytes.len() > FRAME_PART_BYTES {
        let rest = bytes.split_off(FRAME_PART_BYTES);
        out.push(NetFrame { last: false, bytes });
        bytes = rest;
    }
    out.push(NetFrame { last: true, bytes });
    out
}

/// The stream replication's reliable frames ride (spec 120 § 5.1): replication control, so a
/// loss on the game RPC or bulk stream never holds a spawn or despawn back.
pub const BRIDGE_RELIABLE: Wire = Wire::Stream(crate::connection::StreamId::REPLICATION);

/// The stream the runtime's `ReliableUnordered` frames ride (wire 6, spec 120 § 5.1 stream 7,
/// ADR-10052 (3)): the unordered RPC stream, so neither a loss on the replication stream nor one on
/// another unordered frame holds them back. Such a frame is always one part: an unordered stream
/// does not keep parts in order. A larger one (a hand-built frame; the runtime never makes one)
/// rides [`BRIDGE_RELIABLE`].
pub const BRIDGE_UNORDERED: Wire = Wire::Stream(crate::connection::StreamId::UNORDERED_RPC);

fn wire_of(channel: Channel) -> Wire {
    match channel {
        Channel::ReliableUnordered => BRIDGE_UNORDERED,
        Channel::ReliableOrdered => BRIDGE_RELIABLE,
        Channel::Unreliable | Channel::UnreliableSequenced => Wire::Unreliable,
    }
}

fn class_of(wire: Wire) -> Channel {
    if wire == BRIDGE_UNORDERED {
        Channel::ReliableUnordered
    } else if wire.is_reliable() {
        Channel::ReliableOrdered
    } else {
        Channel::Unreliable
    }
}

fn essential(d: &Datagram) -> bool {
    // Oversized frames reach the shedding path before NetFrame encoding can truncate them.
    !d.channel.is_reliable()
        && d.bytes.len() <= FRAME_PART_BYTES
        && matches!(
            d.bytes.first(),
            Some(&crate::runtime::TAG_ACK)
                | Some(&crate::predict::TAG_INPUT)
                | Some(&crate::predict::TAG_INPUT_ACK)
        )
}

/// Reassembly and send backlog of one connection.
#[derive(Debug)]
struct Link {
    assembling: Vec<u8>,
    /// Largest frame this side reassembles.
    frame_limit: usize,
    backlog: VecDeque<(Wire, NetFrame)>,
    backlog_bytes: usize,
    /// The last drain stopped on backpressure: unreliable frames would only go stale in the queue.
    held_back: bool,
    unreliable_shed: u64,
    /// The backlog passed [`MAX_BACKLOG_BYTES`] and was dropped: the reliable stream lost parts,
    /// so this link sends nothing more (a later frame would be reassembled behind the head of a
    /// frame whose tail was dropped).
    overflowed: bool,
}

impl Default for Link {
    fn default() -> Self {
        Self::new(MAX_FRAME_BYTES)
    }
}

impl Link {
    fn new(frame_limit: usize) -> Self {
        Self {
            assembling: Vec::new(),
            frame_limit,
            backlog: VecDeque::new(),
            backlog_bytes: 0,
            held_back: false,
            unreliable_shed: 0,
            overflowed: false,
        }
    }

    /// Adds a received part; returns a whole frame when `part` completes one. `Err` when the frame
    /// grows past this side's limit.
    fn receive(&mut self, wire: Wire, part: NetFrame) -> Result<Option<Vec<u8>>, ()> {
        if wire == Wire::Unreliable {
            // Unreliable frames are always one part.
            return Ok(part.last.then_some(part.bytes));
        }
        if wire == BRIDGE_UNORDERED {
            // So are unordered ones (their parts could arrive in any order): a first part of
            // several is not something an honest peer sends.
            return if part.last { Ok(Some(part.bytes)) } else { Err(()) };
        }
        if self.assembling.len() + part.bytes.len() > self.frame_limit {
            self.assembling.clear();
            return Err(());
        }
        self.assembling.extend_from_slice(&part.bytes);
        if part.last {
            Ok(Some(std::mem::take(&mut self.assembling)))
        } else {
            Ok(None)
        }
    }

    /// Queues a frame's parts. While the endpoint pushes back an unreliable frame is dropped.
    /// `false` when the backlog is past [`MAX_BACKLOG_BYTES`]: the peer has stalled for good, the
    /// backlog is dropped and the link refuses every later frame.
    fn queue(&mut self, d: Datagram) -> bool {
        if self.overflowed {
            return false;
        }
        let mut wire = wire_of(d.channel);
        let len = d.bytes.len();
        // Game events historically used the ordered stream when their tick batch exceeded
        // one datagram. The snapshot no-promotion rule must not erase those events.
        if wire == Wire::Unreliable
            && len > FRAME_PART_BYTES
            && d.bytes.first() == Some(&crate::runtime::TAG_UMSG)
        {
            wire = BRIDGE_RELIABLE;
        }
        if wire == BRIDGE_UNORDERED && len > FRAME_PART_BYTES {
            wire = BRIDGE_RELIABLE;
        }
        if wire == Wire::Unreliable && (self.held_back || len > FRAME_PART_BYTES) {
            self.unreliable_shed += 1;
            return true;
        }
        if self.backlog_bytes + len > MAX_BACKLOG_BYTES {
            self.backlog.clear();
            self.backlog_bytes = 0;
            self.overflowed = true;
            return false;
        }
        self.backlog_bytes += len;
        for p in parts(d.bytes) {
            self.backlog.push_back((wire, p));
        }
        true
    }

    fn forget_front(&mut self) {
        if let Some((_, part)) = self.backlog.pop_front() {
            self.backlog_bytes = self.backlog_bytes.saturating_sub(part.bytes.len());
        }
    }

    /// Sends queued parts until the endpoint pushes back. `false` when the peer is gone or the
    /// link overflowed.
    fn drain(&mut self, mut send: impl FnMut(Wire, &NetFrame) -> Result<(), SendError>) -> bool {
        if self.overflowed {
            return false;
        }
        while let Some((wire, part)) = self.backlog.front() {
            match send(*wire, part) {
                Ok(()) => self.forget_front(),
                Err(SendError::TooLarge { .. }) if *wire == Wire::Unreliable => {
                    if part.bytes.first() == Some(&crate::runtime::TAG_UMSG) {
                        if let Some((wire, _)) = self.backlog.front_mut() {
                            *wire = BRIDGE_RELIABLE;
                        }
                        continue;
                    }
                    // Never turn a snapshot into ordered traffic. The next state tick heals it.
                    self.unreliable_shed += 1;
                    self.forget_front();
                }
                Err(SendError::Backpressure(_)) | Err(SendError::RateLimited) => {
                    self.held_back = true;
                    return true;
                }
                Err(SendError::NotConnected) | Err(SendError::UnknownStream(_)) => {
                    self.backlog.clear();
                    self.backlog_bytes = 0;
                    return false;
                }
                Err(SendError::TooLarge { .. }) => {
                    tracing::warn!(target: "net", "net bridge: a frame part was refused as too large");
                    self.forget_front();
                }
            }
        }
        self.held_back = false;
        true
    }
}

/// Why [`FrameAssembler::receive`] refused a part.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FramePartError {
    /// The part's bytes are not a [`NetFrame`].
    #[error("frame part: {0}")]
    Malformed(WireError),
    /// The frame grew past this side's limit; the bridge closes the link
    /// (`ProtocolError`).
    #[error("frame over the size limit")]
    TooLarge,
    /// A part of several on the unordered stream (wire 6, ADR-10052 (3)): frames there are one
    /// part each, so the bridge closes the link (`ProtocolError` on a client, `MalformedPacket`
    /// on the server).
    #[error("a frame split into parts on the unordered stream")]
    SplitUnordered,
}

/// The bridges' reassembly of received frame parts, on its own (ADR-3283): what [`ServerBridge`]
/// keeps per peer and [`ClientBridge`] keeps for the server, with the same limits. For fuzz targets
/// and tests that feed parts without a transport.
#[derive(Debug)]
pub struct FrameAssembler {
    link: Link,
}

impl FrameAssembler {
    /// A client's assembler: frames from the server, up to [`MAX_FRAME_BYTES`].
    pub fn from_server() -> Self {
        Self {
            link: Link::new(MAX_FRAME_BYTES),
        }
    }

    /// A server's assembler for one peer: frames from a client, up to [`MAX_CLIENT_FRAME_BYTES`].
    pub fn from_client() -> Self {
        Self {
            link: Link::new(MAX_CLIENT_FRAME_BYTES),
        }
    }

    /// Decodes one part as it arrived (the [`NetFrame`] wire form) on the reliable or unreliable
    /// channel; returns the whole frame when the part completes one.
    pub fn receive(&mut self, reliable: bool, part: &[u8]) -> Result<Option<Vec<u8>>, FramePartError> {
        let part = NetFrame::from_bytes(part).map_err(FramePartError::Malformed)?;
        let wire = if reliable {
            BRIDGE_RELIABLE
        } else {
            Wire::Unreliable
        };
        self.link
            .receive(wire, part)
            .map_err(|()| FramePartError::TooLarge)
    }

    /// Decodes one part as it arrived on the unordered RPC stream ([`BRIDGE_UNORDERED`]):
    /// always a whole frame, or [`FramePartError::SplitUnordered`].
    pub fn receive_unordered(&mut self, part: &[u8]) -> Result<Option<Vec<u8>>, FramePartError> {
        let part = NetFrame::from_bytes(part).map_err(FramePartError::Malformed)?;
        self.link
            .receive(BRIDGE_UNORDERED, part)
            .map_err(|()| FramePartError::SplitUnordered)
    }

    /// Bytes of an unfinished frame held now.
    pub fn assembling_bytes(&self) -> usize {
        self.link.assembling.len()
    }
}

/// Connects a server world's net runtime to a `NetServer`.
///
/// [`Self::pump_in`] returns the endpoint's events and, as [`ServerEvent::Rejected`], the
/// refusals the replication runtime decided ([`NetNode::reject`]; KI-NT-024, ADR-10077), so a
/// bridge host reads one stream. A runtime refusal's `error` is `WireError::Invalid` carrying the
/// refusal's detail.
#[derive(Debug, Default)]
pub struct ServerBridge {
    links: HashMap<PeerId, Link>,
    /// Runtime refusals read from the node, not yet returned by `pump_in`.
    runtime_rejections: VecDeque<NetRejection>,
    /// The node's refusal total when the bridge last read it.
    runtime_rejections_seen: u64,
}

impl ServerBridge {
    /// A bridge with no peers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Copies the refusals the node decided since the last read (KI-NT-024, ADR-10077), without
    /// taking them from the node. Ones the host took before this read, or the node's full log
    /// dropped, are not copied; at most [`MAX_KEPT_REJECTIONS`] wait here, oldest dropped first.
    fn read_runtime_rejections(&mut self, world: &World) {
        let Some(node) = world.get_resource::<NetNode>() else {
            return;
        };
        let (total, _) = node.rejection_counts();
        let new = usize::try_from(total.saturating_sub(self.runtime_rejections_seen)).unwrap_or(usize::MAX);
        self.runtime_rejections_seen = total;
        if new == 0 {
            return;
        }
        let kept = node.rejections();
        let skip = kept.len().saturating_sub(new);
        self.runtime_rejections.extend(kept.skip(skip).copied());
        while self.runtime_rejections.len() > MAX_KEPT_REJECTIONS {
            self.runtime_rejections.pop_front();
        }
    }

    /// After `server.receive(now)`: handles the endpoint's events (joins, leaves, frames) and fills
    /// the node's inbox. Returns the join and leave events for the game, and every refusal: the
    /// runtime's, decided since the last pump, first (as [`ServerEvent::Rejected`], so a fatal one
    /// comes before the `PeerLeft` it causes; KI-NT-024, ADR-10077), then the endpoint's.
    /// The runtime's stay readable through [`NetNode::take_rejections`] too.
    pub fn pump_in<G, S>(&mut self, world: &mut World, server: &mut NetServer<G, S>) -> Vec<ServerEvent<G>>
    where
        G: NetGame<Message = NetFrame>,
        S: DatagramSocket,
    {
        ensure_resources(world);
        self.read_runtime_rejections(world);
        let mut out: Vec<ServerEvent<G>> = self
            .runtime_rejections
            .drain(..)
            .map(|r| ServerEvent::Rejected {
                peer: r.peer,
                kind: r.kind,
                error: WireError::Invalid(r.detail),
            })
            .collect();
        let mut bad: Vec<PeerId> = Vec::new();
        while let Some(event) = server.poll_event() {
            match event {
                ServerEvent::PeerJoined { peer, .. } => {
                    server_add_peer(world, peer);
                    if let Some(max) = server.max_unreliable_message_bytes(peer) {
                        world
                            .resource_mut::<crate::replication::ServerReplication>()
                            .set_transport_frame_bytes(peer, max.saturating_sub(3));
                    }
                    self.links.insert(peer, Link::new(MAX_CLIENT_FRAME_BYTES));
                    out.push(event);
                }
                ServerEvent::PeerLeft { peer, .. } => {
                    if crate::runtime::server_peer_is_dormant(world, peer) {
                        // The grace ended (spec 120 § 10.3): the game's hook decides.
                        crate::resume::server_grace_expired(world, peer);
                    } else {
                        server_remove_peer(world, peer);
                    }
                    self.links.remove(&peer);
                    out.push(event);
                }
                ServerEvent::PeerDormant { peer, .. } => {
                    // Spec 120 § 10.3: entities, ownership and interest stay; the link goes.
                    crate::resume::server_peer_dormant(world, peer);
                    self.links.remove(&peer);
                    out.push(event);
                }
                ServerEvent::PeerResumed { peer, .. } => {
                    crate::resume::server_peer_resumed(world, peer, server.config().resume_grace_ms);
                    // A fresh reassembly and backlog: the old connection's half frames are gone.
                    self.links.insert(peer, Link::new(MAX_CLIENT_FRAME_BYTES));
                    out.push(event);
                }
                ServerEvent::Message {
                    peer,
                    channel,
                    message,
                } => {
                    let Some(link) = self.links.get_mut(&peer) else {
                        continue;
                    };
                    match link.receive(channel, message) {
                        Ok(Some(bytes)) => world.resource_mut::<NetNode>().inbox.push(Datagram {
                            peer,
                            channel: class_of(channel),
                            bytes,
                        }),
                        Ok(None) => {}
                        Err(()) => bad.push(peer),
                    }
                }
                // The server already scored it (or removed the peer); the host decides the rest.
                ServerEvent::PeerInterrupted { peer } => {
                    crate::model::mark_peer_interrupted(world, peer, true);
                    out.push(event);
                }
                ServerEvent::PeerRestored { peer } => {
                    crate::model::mark_peer_interrupted(world, peer, false);
                    out.push(event);
                }
                ServerEvent::Rejected { .. } | ServerEvent::QualityChanged { .. } => out.push(event),
            }
        }
        for peer in bad {
            crate::runtime::server_kick(world, peer, DisconnectReason::MalformedPacket, "frame too large");
        }
        // The state frame budget follows the link's path MTU (spec 120 § 4.3, ADR-8300): read it
        // again every frame, so a probed size is used at once and a black hole shrinks the next
        // frame. Without `mtu_probing` the value never changes.
        // Only a change is written: the resource's change detection stays quiet otherwise.
        let changed: Vec<(PeerId, usize)> = world
            .get_resource::<crate::replication::ServerReplication>()
            .map(|replication| {
                self.links
                    .keys()
                    .filter_map(|peer| {
                        let bytes = server.max_unreliable_message_bytes(*peer)?.saturating_sub(3);
                        (replication.transport_frame_bytes(*peer) != Some(bytes)).then_some((*peer, bytes))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !changed.is_empty()
            && let Some(mut replication) = world.get_resource_mut::<crate::replication::ServerReplication>()
        {
            for (peer, bytes) in changed {
                replication.set_transport_frame_bytes(peer, bytes);
            }
        }
        // The ticks that follow size each connection's frames by the transport's allowance (spec
        // 120 § 4.2 item 9).
        if let Some(mut rep) = world.get_resource_mut::<crate::replication::ServerReplication>() {
            for peer in self.links.keys() {
                rep.set_send_allowance(*peer, server.send_allowance(*peer));
            }
        }
        out
    }

    /// After the app's fixed ticks, before `server.flush(now)`: applies the node's disconnects and
    /// queues its frames on the endpoint.
    pub fn pump_out<G, S>(&mut self, world: &mut World, server: &mut NetServer<G, S>)
    where
        G: NetGame<Message = NetFrame>,
        S: DatagramSocket,
    {
        // Read the refusals of this pass now: a host that takes them after `pump_out` still sees
        // them as events at the next `pump_in`.
        self.read_runtime_rejections(world);
        let Some(mut node) = world.get_resource_mut::<NetNode>() else {
            return;
        };
        let disconnects = std::mem::take(&mut node.disconnects);
        let outbox = std::mem::take(&mut node.outbox);
        for d in disconnects {
            server.kick(d.peer, d.reason);
            self.links.remove(&d.peer);
        }
        let mut stalled: Vec<PeerId> = Vec::new();
        for d in outbox {
            let peer = d.peer;
            if essential(&d) {
                let part = NetFrame {
                    last: true,
                    bytes: d.bytes,
                };
                if server.send_essential_message(peer, &part).is_err()
                    && let Some(link) = self.links.get_mut(&peer)
                {
                    link.unreliable_shed += 1;
                }
                continue;
            }
            if let Some(link) = self.links.get_mut(&peer)
                && !link.queue(d)
                && !stalled.contains(&peer)
            {
                stalled.push(peer);
            }
        }
        for peer in stalled {
            tracing::warn!(target: "net", peer = peer.0, "net bridge: send backlog over its limit");
            self.links.remove(&peer);
            server_remove_peer(world, peer);
            server.kick(peer, DisconnectReason::ReliableOverflow);
        }
        let peers: Vec<PeerId> = self.links.keys().copied().collect();
        for peer in peers {
            let Some(link) = self.links.get_mut(&peer) else {
                continue;
            };
            if !link.drain(|wire, part| server.send(peer, wire, part)) {
                self.links.remove(&peer);
            }
        }
    }

    /// Parts waiting for the endpoint (backpressure), per peer.
    pub fn backlog(&self, peer: PeerId) -> usize {
        self.links.get(&peer).map_or(0, |l| l.backlog.len())
    }

    /// Bytes waiting for the endpoint, per peer.
    pub fn backlog_bytes(&self, peer: PeerId) -> usize {
        self.links.get(&peer).map_or(0, |l| l.backlog_bytes)
    }

    /// Unreliable frames dropped for this peer while the endpoint pushed back.
    pub fn unreliable_shed(&self, peer: PeerId) -> u64 {
        self.links.get(&peer).map_or(0, |l| l.unreliable_shed)
    }
}

/// Connects a client world's net runtime to a `NetClient`.
#[derive(Debug, Default)]
pub struct ClientBridge {
    link: Link,
}

impl ClientBridge {
    /// A bridge for one client.
    pub fn new() -> Self {
        Self::default()
    }

    /// After `client.receive(now)`: handles the endpoint's events and fills the node's inbox. A
    /// connect sets `LocalPeer`; an end sets `NetNode::closed`. Returns those events for the game.
    pub fn pump_in<G, S>(&mut self, world: &mut World, client: &mut NetClient<G, S>) -> Vec<ClientEvent<G>>
    where
        G: NetGame<Message = NetFrame>,
        S: DatagramSocket,
    {
        ensure_resources(world);
        let mut out = Vec::new();
        while let Some(event) = client.poll_event() {
            match event {
                ClientEvent::PeerInterrupted
                | ClientEvent::PeerRestored
                | ClientEvent::InputsLate { .. }
                | ClientEvent::QualityChanged { .. } => out.push(event),
                ClientEvent::Connected { peer, .. } => {
                    world.insert_resource(LocalPeer(Some(peer)));
                    // A resumed connection (spec 120 § 10.3) starts open again.
                    world.resource_mut::<NetNode>().closed = None;
                    crate::replication::client_session_started(world);
                    out.push(event);
                }
                ClientEvent::Ended(ref end) => {
                    let reason = match end {
                        ClientEnd::Disconnected(r) => *r,
                        _ => DisconnectReason::Left,
                    };
                    let mut node = world.resource_mut::<NetNode>();
                    if self.link.overflowed {
                        // This bridge ended the connection itself; keep that reason.
                        node.closed.get_or_insert(reason);
                    } else {
                        node.closed = Some(reason);
                    }
                    out.push(event);
                }
                ClientEvent::VersionMismatch { .. } => out.push(event),
                ClientEvent::Message { channel, message } => match self.link.receive(channel, message) {
                    Ok(Some(bytes)) => world.resource_mut::<NetNode>().inbox.push(Datagram {
                        peer: PeerId::SERVER,
                        channel: class_of(channel),
                        bytes,
                    }),
                    Ok(None) => {}
                    Err(()) => {
                        tracing::warn!(target: "net", "net bridge: the server sent a frame over the size limit or split on the unordered stream");
                        world.resource_mut::<NetNode>().closed = Some(DisconnectReason::ProtocolError);
                    }
                },
            }
        }
        out
    }

    /// After the app's fixed ticks, before `client.flush(now)`: queues the node's frames.
    ///
    /// When the send backlog passes [`MAX_BACKLOG_BYTES`] (nobody is taking these frames: the
    /// connection never came up, or the server stopped acknowledging), the client ends its
    /// connection with `NetNode::closed` = `ReliableOverflow`, as the server kicks such a peer.
    /// Frames queued after that are dropped: the reliable stream already lost parts, so a later
    /// frame would reach the server spliced onto the head of a frame whose tail was dropped.
    pub fn pump_out<G, S>(&mut self, world: &mut World, client: &mut NetClient<G, S>)
    where
        G: NetGame<Message = NetFrame>,
        S: DatagramSocket,
    {
        let Some(mut node) = world.get_resource_mut::<NetNode>() else {
            return;
        };
        let outbox = std::mem::take(&mut node.outbox);
        if self.link.overflowed {
            return;
        }
        for d in outbox {
            // Inputs and feedback bypass both reliable backlogs.
            if essential(&d) {
                let part = NetFrame {
                    last: true,
                    bytes: d.bytes,
                };
                if client.send_essential_message(&part).is_err() {
                    self.link.unreliable_shed += 1;
                }
                continue;
            }
            if !self.link.queue(d) {
                tracing::warn!(target: "net", "net bridge: send backlog over its limit; ending the connection");
                node.closed.get_or_insert(DisconnectReason::ReliableOverflow);
                client.disconnect();
                return;
            }
        }
        if !client.is_connected() {
            return;
        }
        self.link.drain(|wire, part| client.send(wire, part));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_ack_bypasses_the_bridges_reliable_backlog() {
        struct Game;
        impl NetGame for Game {
            type Message = NetFrame;
            type Input = u8;
            type JoinData = ();
            const GAME_ID: &'static str = "regia.bridge_ack_test";
            const PROTOCOL_VERSION: u32 = 1;
        }
        let net = crate::LoopbackNet::new();
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 7777));
        let mut server: NetServer<Game, _> = NetServer::with_secret(
            net.bind(addr).expect("bind"),
            crate::ServerConfig::default(),
            [3; 32],
        );
        let mut client: NetClient<Game, _> = NetClient::connect(
            net.bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                .expect("bind"),
            addr,
            crate::ClientConfig {
                link: crate::LinkConfig {
                    max_reliable_in_flight: 1,
                    ..crate::LinkConfig::default()
                },
                ..crate::ClientConfig::default()
            },
            &(),
        )
        .expect("join");
        for now in 0..20 {
            client.update(now);
            server.update(now);
        }
        assert!(client.is_connected());
        // Fill the window of the stream the bridge's reliable frames ride (admission is per
        // stream since ADR-8302).
        client
            .send(
                BRIDGE_RELIABLE,
                &NetFrame {
                    last: true,
                    bytes: vec![8],
                },
            )
            .expect("fill window");
        let mut bridge = ClientBridge::new();
        assert!(bridge.link.queue(frame(Channel::ReliableOrdered, 1)));
        assert!(bridge.link.drain(|wire, part| client.send(wire, part)));
        assert!(bridge.link.held_back);
        let mut world = World::new();
        ensure_resources(&mut world);
        world.resource_mut::<NetNode>().outbox.push(Datagram {
            peer: PeerId::SERVER,
            channel: Channel::Unreliable,
            bytes: vec![4, 0],
        });
        bridge.pump_out(&mut world, &mut client);
        client.flush(21);
        server.receive(21);
        assert!(
            std::iter::from_fn(|| server.poll_event()).any(|event| matches!(event,
            ServerEvent::Message { channel: Wire::Unreliable, message, .. } if message.bytes == [4, 0])),
            "the state ack reaches the server despite both reliable backlogs"
        );
        assert!(bridge.link.held_back, "the reliable backlog is still present");
    }

    #[test]
    fn oversized_unreliable_frames_are_never_promoted() {
        let mut link = Link::default();
        assert!(link.queue(frame(Channel::Unreliable, 1400)));
        let mut attempts = Vec::new();
        assert!(link.drain(|wire, part| {
            attempts.push(wire);
            Err(SendError::TooLarge {
                len: part.bytes.len(),
                max: 1150,
            })
        }));
        assert_eq!(attempts, vec![Wire::Unreliable], "no retry on the ordered stream");
        assert!(link.backlog.is_empty());
        assert_eq!(link.unreliable_shed, 1);
        assert!(link.queue(frame(Channel::Unreliable, FRAME_PART_BYTES + 1)));
        assert!(
            link.backlog.is_empty(),
            "a multi-part unreliable frame is omitted"
        );
        assert_eq!(link.unreliable_shed, 2);
    }

    /// Wire 6 (ADR-10052 (3)): a `ReliableUnordered` frame rides stream 7, one part, and
    /// arrives as that class; one past a part rides the replication stream; a split part on
    /// stream 7 is refused.
    #[test]
    fn unordered_frames_ride_stream_seven_as_one_part() {
        let mut link = Link::default();
        assert!(link.queue(frame(Channel::ReliableUnordered, 300)));
        assert!(link.queue(frame(Channel::ReliableOrdered, 300)));
        assert!(link.queue(frame(Channel::ReliableUnordered, FRAME_PART_BYTES + 1)));
        let wires: Vec<Wire> = link.backlog.iter().map(|(w, _)| *w).collect();
        assert_eq!(
            wires,
            vec![
                BRIDGE_UNORDERED,
                BRIDGE_RELIABLE,
                BRIDGE_RELIABLE,
                BRIDGE_RELIABLE
            ],
            "unordered on stream 7; the oversized one in two parts on stream 3"
        );
        assert_eq!(BRIDGE_UNORDERED, Wire::Stream(crate::connection::StreamId(7)));
        assert_eq!(class_of(BRIDGE_UNORDERED), Channel::ReliableUnordered);
        assert_eq!(class_of(BRIDGE_RELIABLE), Channel::ReliableOrdered);
        let mut rx = FrameAssembler::from_server();
        let whole = NetFrame {
            last: true,
            bytes: vec![9, 1, 0],
        };
        assert_eq!(
            rx.receive_unordered(&whole.to_bytes().expect("codec")),
            Ok(Some(vec![9, 1, 0]))
        );
        let split = NetFrame {
            last: false,
            bytes: vec![9, 1],
        };
        assert_eq!(
            rx.receive_unordered(&split.to_bytes().expect("codec")),
            Err(FramePartError::SplitUnordered)
        );
        assert_eq!(rx.assembling_bytes(), 0, "nothing of it is held");
    }

    #[test]
    fn frames_split_and_reassemble() {
        let bytes: Vec<u8> = (0..(FRAME_PART_BYTES * 2 + 7)).map(|i| i as u8).collect();
        let ps = parts(bytes.clone());
        assert_eq!(ps.len(), 3);
        assert!(ps[2].last && !ps[0].last && !ps[1].last);
        assert!(
            ps.iter().all(|p| p.bytes.len() <= FRAME_PART_BYTES),
            "every part stays within FRAME_PART_BYTES"
        );
        let mut link = Link::default();
        let mut whole = None;
        for p in ps {
            let round = NetFrame::from_bytes(&p.to_bytes().expect("codec")).expect("codec");
            whole = link.receive(Wire::GAME_RPC, round).expect("in bounds");
        }
        assert_eq!(whole, Some(bytes));
        assert_eq!(parts(Vec::new()).len(), 1, "an empty frame is one part");
    }

    /// `queue` moves the datagram's buffer into its parts: one part keeps that allocation, and a
    /// 32 KiB datagram becomes two parts whose first buffer is still the source.
    #[test]
    fn queue_moves_the_datagram_buffer_into_its_parts() {
        let mut bytes = vec![7u8; 100];
        bytes.shrink_to_fit();
        let ptr = bytes.as_ptr();
        let mut link = Link::default();
        assert!(link.queue(Datagram {
            peer: PeerId(1),
            channel: Channel::ReliableOrdered,
            bytes,
        }));
        let (_, part) = link.backlog.front().expect("one part");
        assert!(part.last);
        assert_eq!(part.bytes.len(), 100);
        assert_eq!(part.bytes.as_ptr(), ptr, "one part keeps the datagram allocation");

        let mut bytes = vec![9u8; 32 * 1024];
        bytes.shrink_to_fit();
        let ptr = bytes.as_ptr();
        let mut link = Link::default();
        assert!(link.queue(Datagram {
            peer: PeerId(1),
            channel: Channel::ReliableOrdered,
            bytes,
        }));
        assert_eq!(link.backlog.len(), 2, "32 KiB is two parts");
        assert_eq!(
            link.backlog[0].1.bytes.as_ptr(),
            ptr,
            "the first part is the source buffer"
        );
        assert_eq!(link.backlog[0].1.bytes.len(), FRAME_PART_BYTES);
        assert_eq!(link.backlog[1].1.bytes.len(), 32 * 1024 - FRAME_PART_BYTES);
        assert!(link.backlog[1].1.last && !link.backlog[0].1.last);
        assert_eq!(
            link.backlog[0].0, BRIDGE_RELIABLE,
            "a reliable frame retains its channel"
        );
        assert_eq!(link.backlog[0].1.bytes[0], 9);
        assert_eq!(link.backlog[1].1.bytes[0], 9);
    }

    #[test]
    fn oversized_frames_are_refused() {
        let mut link = Link::default();
        let big = NetFrame {
            last: false,
            bytes: vec![0; FRAME_PART_BYTES],
        };
        let mut result = Ok(None);
        for _ in 0..=(MAX_FRAME_BYTES / FRAME_PART_BYTES) {
            result = link.receive(Wire::GAME_RPC, big.clone());
            if result.is_err() {
                break;
            }
        }
        assert!(result.is_err());
        assert!(NetFrame::from_bytes(&[2, 0, 0]).is_err(), "bad part flag");
    }

    /// Audit L2 (zai), H4 (grok): a client cannot pin 8 MiB per slot with an unfinished frame.
    #[test]
    fn a_client_frame_has_a_smaller_limit() {
        let mut link = Link::new(MAX_CLIENT_FRAME_BYTES);
        let part = NetFrame {
            last: false,
            bytes: vec![0; FRAME_PART_BYTES],
        };
        let fits = MAX_CLIENT_FRAME_BYTES / FRAME_PART_BYTES;
        for _ in 0..fits {
            assert!(link.receive(Wire::GAME_RPC, part.clone()).is_ok());
        }
        assert!(link.receive(Wire::GAME_RPC, part).is_err());
        assert!(link.assembling.is_empty(), "the buffer is released");
    }

    fn frame(channel: Channel, len: usize) -> Datagram {
        Datagram {
            peer: PeerId(1),
            channel,
            bytes: vec![0; len],
        }
    }

    /// Audit H3 (muse) / M6 (muse2): a peer that stops acknowledging cannot grow the backlog past
    /// the bound, and the caller learns it must drop the peer.
    #[test]
    fn the_backlog_is_bounded() {
        let mut link = Link::default();
        let mut queued = 0;
        while link.queue(frame(Channel::ReliableOrdered, 1024 * 1024)) {
            queued += 1;
            assert!(link.backlog_bytes <= MAX_BACKLOG_BYTES);
            assert!(queued <= 64, "never refused");
        }
        assert_eq!(queued, MAX_BACKLOG_BYTES / (1024 * 1024));
        assert_eq!((link.backlog.len(), link.backlog_bytes), (0, 0), "released");
    }

    /// Audit net3-correctness #1 (grok): after an overflow dropped the tail of a frame whose head
    /// was already sent, no later frame is queued or sent on that link.
    #[test]
    fn an_overflowed_link_sends_nothing_more() {
        let mut link = Link::default();
        // A frame of two parts: the endpoint takes the head, then pushes back.
        assert!(link.queue(frame(Channel::ReliableOrdered, FRAME_PART_BYTES + 10)));
        let mut sent: Vec<NetFrame> = Vec::new();
        assert!(link.drain(|_, part| {
            if sent.is_empty() {
                sent.push(part.clone());
                Ok(())
            } else {
                Err(SendError::Backpressure(1))
            }
        }));
        assert_eq!(sent.len(), 1);
        assert!(!sent[0].last, "only the head went out");
        // Fill the backlog to just under its limit, then overflow it.
        let fill = MAX_BACKLOG_BYTES - link.backlog_bytes - 100;
        assert!(link.queue(frame(Channel::ReliableOrdered, fill)));
        assert!(!link.queue(frame(Channel::ReliableOrdered, 200)));
        assert_eq!((link.backlog.len(), link.backlog_bytes), (0, 0), "released");
        // A later small frame is refused and nothing more reaches the endpoint.
        assert!(!link.queue(frame(Channel::ReliableOrdered, 50)));
        assert!(!link.queue(frame(Channel::Unreliable, 50)));
        assert!(!link.drain(|_, part| {
            sent.push(part.clone());
            Ok(())
        }));
        assert_eq!(sent.len(), 1, "no part follows the orphaned head");
    }

    /// Unreliable frames are dropped while the endpoint pushes back, kept otherwise; the byte count
    /// follows sends.
    #[test]
    fn unreliable_frames_are_shed_under_backpressure_only() {
        let mut link = Link::default();
        assert!(link.queue(frame(Channel::ReliableOrdered, 10)));
        assert!(
            link.queue(frame(Channel::Unreliable, 10)),
            "kept: no backpressure yet"
        );
        assert_eq!(link.backlog.len(), 2);
        assert!(link.drain(|_, _| Err(SendError::Backpressure(1))));
        assert!(link.queue(frame(Channel::Unreliable, 10)));
        assert_eq!((link.backlog.len(), link.unreliable_shed), (2, 1));
        assert_eq!(link.backlog_bytes, 20);
        assert!(link.drain(|_, _| Ok(())));
        assert_eq!((link.backlog.len(), link.backlog_bytes), (0, 0));
        assert!(link.queue(frame(Channel::Unreliable, 10)), "the pushback ended");
        assert_eq!(link.unreliable_shed, 1);
    }
}
