// SPDX-License-Identifier: MPL-2.0
//! Net messages (spec 70 § 6): typed one-off calls between processes, attached to a replicated
//! entity or to the session, with a direction, a channel, validation, ordering against state,
//! queue bounds and flood limits.
//!
//! Declare a message by implementing [`NetMessage`] (plus the marker trait of its direction,
//! [`ToServerMessage`], [`ToOwnerMessage`] or [`BroadcastMessage`]), register it with
//! `NetMessageAppExt::add_net_message`, send it through `NetSender` and read it in a handler
//! system through `NetMessages`.
//!
//! The wire half is core (spec 120 § 3.2, ADR-8501): the trait, its direction and channel, the
//! validation verdict, the stable id, the counters and [`NetMessageRegistry`] (registration order,
//! protocol hash, per-type fingerprints and [`NetMessageRegistry::decode`]) build without the `ecs`
//! feature, so a game that runs its own simulation can declare messages with `#[derive(NetMessage)]`
//! and carry them in its own payloads. The App extension, the `NetSender` system parameter, the
//! `NetMessages` resource and the entity and world fields of [`ValidateContext`] need `ecs`.

use std::any::{Any, TypeId};
use std::collections::HashMap;

#[cfg(feature = "ecs")]
use regia_ecs::ecs::system::SystemParam;
#[cfg(feature = "ecs")]
use regia_ecs::prelude::*;
use thiserror::Error;

#[cfg(feature = "ecs")]
use crate::bits::BitWriter;
use crate::bits::{BitReader, DecodeError};
use crate::codec::{CodecContext, LayoutHasher, ReplicateCodec};
#[cfg(feature = "ecs")]
use crate::model::{NetId, NetMode};
use crate::model::{NetTick, PeerId};
#[cfg(feature = "ecs")]
use crate::runtime::{NetEntities, NetNode, OutMessage, Recipient};
use crate::settings::RateLimit;

/// Largest encoded message (spec 70 § 6.5). Larger data uses a stream (spec 77 § 8).
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Unreliable messages kept per target entity per connection per tick; older ones are dropped.
pub const MAX_UNRELIABLE_PER_TARGET: usize = 16;
/// Broadcasts the server sends per tick; later ones are dropped.
pub const MAX_BROADCASTS_PER_TICK: usize = 1024;
/// Reliable messages queued per connection before it is disconnected with `ReliableOverflow`.
pub const MAX_RELIABLE_QUEUE: usize = 4096;
/// Reliable bytes queued per connection before it is disconnected with `ReliableOverflow`.
pub const MAX_RELIABLE_QUEUE_BYTES: usize = 1024 * 1024;

/// Who a message goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Client to server. Sent on an entity the sender owns, or on the session.
    ToServer,
    /// Server to the owning client of the target entity, or to one named peer for session messages.
    ToOwner,
    /// Server to every client for which the target entity is relevant, and to the server itself.
    Broadcast,
}

impl Direction {
    /// Lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ToServer => "to_server",
            Self::ToOwner => "to_owner",
            Self::Broadcast => "broadcast",
        }
    }
}

/// The delivery class of a message (spec 70 § 6.5). The transport has two physical channels:
/// both reliable classes ride its reliable-ordered stream (a valid arrival order for
/// `ReliableUnordered`), both unreliable classes its unreliable one; `UnreliableSequenced` adds a
/// per (type, target) sequence filter at the receiver (ADR-1643).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// Exactly once, in send order per connection.
    ReliableOrdered,
    /// Exactly once, in arrival order.
    ReliableUnordered,
    /// At most once, any order.
    Unreliable,
    /// At most once; older than the newest received of the same type and target is dropped.
    UnreliableSequenced,
}

impl Channel {
    /// Whether the class is reliable.
    pub fn is_reliable(self) -> bool {
        matches!(self, Self::ReliableOrdered | Self::ReliableUnordered)
    }

    /// Lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReliableOrdered => "reliable_ordered",
            Self::ReliableUnordered => "reliable_unordered",
            Self::Unreliable => "unreliable",
            Self::UnreliableSequenced => "unreliable_sequenced",
        }
    }
}

/// A validation decision (spec 70 § 6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Deliver.
    Accept,
    /// Drop and count `rejected`.
    Reject(&'static str),
    /// Drop and disconnect the sender with `InvalidMessage`.
    Kick(&'static str),
}

/// What `validate` sees: the sender, the target, the read-only world and the tick. Without the
/// `ecs` feature it carries only the sender and the tick (ADR-8501); `ValidateContext::core`
/// builds one.
pub struct ValidateContext<'a> {
    /// The sending peer.
    pub from: PeerId,
    /// The target entity (`None` for session messages).
    #[cfg(feature = "ecs")]
    pub target: Option<Entity>,
    /// The server world, read-only.
    #[cfg(feature = "ecs")]
    pub world: &'a World,
    /// The tick processing the message.
    pub tick: NetTick,
    #[cfg(not(feature = "ecs"))]
    lifetime: std::marker::PhantomData<&'a ()>,
}

#[cfg(not(feature = "ecs"))]
impl ValidateContext<'_> {
    /// The context of a message from `from` processed at `tick`, for a host that validates
    /// messages outside the ECS.
    pub fn core(from: PeerId, tick: NetTick) -> Self {
        Self {
            from,
            tick,
            lifetime: std::marker::PhantomData,
        }
    }
}

/// A message type (spec 70 § 6.1). Encoded with [`ReplicateCodec`], so `Entity` fields travel as
/// `NetId`.
pub trait NetMessage: ReplicateCodec + Send + Sync + 'static {
    /// Who the message goes to.
    const DIRECTION: Direction;
    /// The delivery class.
    const CHANNEL: Channel = Channel::ReliableOrdered;
    /// Inbound flood limit per sending peer. `None` = the session default.
    const RATE_LIMIT: Option<RateLimit> = None;
    /// Always admitted by the flood guard (still counted). For tiny, essential messages only.
    const ALWAYS_ADMIT: bool = false;
    /// Client side, for proxy targets: hold until the proxy is shown at the sent tick.
    const ALIGN_TO_VIEW: bool = false;
    /// Broadcast only: not delivered to the target's owner when the owner predicts it.
    const SKIP_PREDICTING_OWNER: bool = false;
    /// The schema version (`#[net(version = n)]`, default 1). It feeds the protocol hash, so a
    /// client and a server built with different versions of one message cannot join each other.
    const VERSION: u32 = 1;

    /// The message's stable id: the name it is registered, hashed and found by. The default is
    /// the type path (`std::any::type_name`); `#[derive(NetMessage)]` writes the module path plus
    /// the type name, or the `#[net(id = "..")]` string, which survives renaming and moving the
    /// type.
    fn message_id() -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Server side, to-server messages only: decide before any handler runs.
    fn validate(&self, _cx: &ValidateContext) -> Verdict {
        Verdict::Accept
    }
}

/// The 64-bit stable id of a message id string: the first 8 bytes of its BLAKE3 hash
/// (length-prefixed, through [`LayoutHasher`]), so it is the same on every build and platform.
pub fn message_stable_id(message_id: &str) -> u64 {
    let mut h = LayoutHasher::new();
    h.write_str("net_message");
    h.write_str(message_id);
    h.finish()
}

/// Marker: a [`NetMessage`] whose `DIRECTION` is `ToServer` (bounds `NetSender::send_to_server`).
pub trait ToServerMessage: NetMessage {}
/// Marker: a [`NetMessage`] whose `DIRECTION` is `ToOwner` (bounds `NetSender::send_to_owner`).
pub trait ToOwnerMessage: NetMessage {}
/// Marker: a [`NetMessage`] whose `DIRECTION` is `Broadcast` (bounds `NetSender::broadcast`).
pub trait BroadcastMessage: NetMessage {}

/// Why a send did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum NetSendError {
    /// The target entity has no `NetId`.
    #[error("the target entity is not replicated")]
    NotReplicated,
    /// A server-only send on a client.
    #[error("only the server sends this message")]
    NotServer,
    /// A client-only send on a dedicated server.
    #[error("only a client sends this message")]
    NotClient,
    /// The encoded message is larger than 64 KiB.
    #[error("the encoded message is larger than 64 KiB")]
    TooLarge,
    /// The message type was not registered with `add_net_message`.
    #[error("the message type is not registered")]
    Unregistered,
    /// The marker trait and the `DIRECTION` constant disagree.
    #[error("the message's DIRECTION does not match the send method")]
    WrongDirection,
    /// The named peer is not connected.
    #[error("the peer is not connected")]
    UnknownPeer,
}

/// Per message type counters (spec 70 § 9 `net.messages`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MessageCounters {
    /// Messages handed to the transport (or the local queue).
    pub sent: u64,
    /// Messages that arrived.
    pub received: u64,
    /// Messages a handler saw.
    pub delivered: u64,
    /// Dropped by `validate`.
    pub rejected: u64,
    /// Dropped by the flood guard.
    pub rate_limited: u64,
    /// Dropped because the sender does not own the target.
    pub not_owner: u64,
    /// Dropped because the target never spawned (or was despawned).
    pub unresolved: u64,
    /// Dropped by a queue bound.
    pub dropped_overflow: u64,
    /// Delivered after newer state of the target was applied.
    pub late: u64,
    /// Payload bytes handed to the transport (one copy per receiving peer; local delivery is free).
    pub bytes_sent: u64,
    /// Payload bytes of messages that arrived.
    pub bytes_received: u64,
}

/// One received message as a handler sees it.
#[cfg(feature = "ecs")]
#[derive(Debug, Clone)]
pub struct Received<M> {
    /// The message.
    pub msg: M,
    /// The target entity (`None` for session messages).
    pub target: Option<Entity>,
    /// The sending peer.
    pub from: PeerId,
    /// The sender's tick.
    pub sent_tick: NetTick,
    /// Newer state of the target was already applied when it ran.
    pub late: bool,
}

/// The messages of type `M` delivered this tick. Filled at `NetSet::Apply`, read by handler
/// systems in `FixedUpdate`, cleared at the next `NetSet::Apply`.
#[cfg(feature = "ecs")]
#[derive(Resource, Debug)]
pub struct NetMessages<M: NetMessage> {
    items: Vec<Received<M>>,
}

#[cfg(feature = "ecs")]
impl<M: NetMessage> Default for NetMessages<M> {
    fn default() -> Self {
        Self { items: Vec::new() }
    }
}

#[cfg(feature = "ecs")]
impl<M: NetMessage> NetMessages<M> {
    /// This tick's messages, in delivery order.
    pub fn iter(&self) -> impl Iterator<Item = &Received<M>> {
        self.items.iter()
    }

    /// How many arrived this tick.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether none arrived this tick.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Clears the tick's messages (the net layer calls this at `NetSet::Apply`).
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Adds one (the net layer calls this).
    pub fn push(&mut self, item: Received<M>) {
        self.items.push(item);
    }
}

/// A decoded message waiting for delivery, type-erased.
pub type ErasedMessage = Box<dyn Any + Send + Sync>;

/// Everything the net layer knows about one registered message type.
#[derive(Clone)]
pub struct MessageType {
    /// The message's stable id ([`NetMessage::message_id`]; the type path by default).
    pub name: &'static str,
    /// [`message_stable_id`] of `name`: a 64-bit id that is the same on every build.
    pub stable_id: u64,
    /// The schema version ([`NetMessage::VERSION`]).
    pub version: u32,
    /// Who it goes to.
    pub direction: Direction,
    /// Its delivery class.
    pub channel: Channel,
    /// Its rate limit (`None` = the session default).
    pub rate: Option<RateLimit>,
    /// Skips the flood guard.
    pub always_admit: bool,
    /// Held until the proxy view catches up.
    pub align_to_view: bool,
    /// Not delivered to a predicting owner.
    pub skip_predicting_owner: bool,
    /// The codec layout hash.
    pub layout: u64,
    pub(crate) decode: fn(&mut BitReader, &CodecContext) -> Result<ErasedMessage, DecodeError>,
    pub(crate) validate: fn(&(dyn Any + Send + Sync), &ValidateContext) -> Verdict,
    #[cfg(feature = "ecs")]
    pub(crate) deliver: fn(&mut World, ErasedMessage, Option<Entity>, PeerId, NetTick, bool),
    #[cfg(feature = "ecs")]
    pub(crate) clear: fn(&mut World),
}

impl std::fmt::Debug for MessageType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageType")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("direction", &self.direction)
            .field("channel", &self.channel)
            .finish()
    }
}

fn decode_erased<M: NetMessage>(r: &mut BitReader, cx: &CodecContext) -> Result<ErasedMessage, DecodeError> {
    Ok(Box::new(M::decode(r, cx)?))
}

fn validate_erased<M: NetMessage>(msg: &(dyn Any + Send + Sync), cx: &ValidateContext) -> Verdict {
    match msg.downcast_ref::<M>() {
        Some(m) => m.validate(cx),
        None => Verdict::Reject("message type mismatch"),
    }
}

#[cfg(feature = "ecs")]
fn deliver_erased<M: NetMessage>(
    world: &mut World,
    msg: ErasedMessage,
    target: Option<Entity>,
    from: PeerId,
    sent_tick: NetTick,
    late: bool,
) {
    let Ok(msg) = msg.downcast::<M>() else {
        return;
    };
    world
        .get_resource_or_insert_with(NetMessages::<M>::default)
        .push(Received {
            msg: *msg,
            target,
            from,
            sent_tick,
            late,
        });
}

#[cfg(feature = "ecs")]
fn clear_erased<M: NetMessage>(world: &mut World) {
    if let Some(mut q) = world.get_resource_mut::<NetMessages<M>>() {
        q.clear();
    }
}

/// The registered message types, in registration order (the order feeds the protocol hash).
#[cfg_attr(feature = "ecs", derive(Resource))]
#[derive(Debug, Default, Clone)]
pub struct NetMessageRegistry {
    types: Vec<MessageType>,
    by_type: HashMap<TypeId, u16>,
    /// Counters per registered type, by index.
    pub counters: Vec<MessageCounters>,
}

impl NetMessageRegistry {
    /// Registers `M` (once; a second registration is ignored). Returns its index.
    pub fn register<M: NetMessage>(&mut self) -> u16 {
        if let Some(i) = self.by_type.get(&TypeId::of::<M>()) {
            return *i;
        }
        let mut h = LayoutHasher::new();
        M::layout(&mut h);
        let index = self.types.len() as u16;
        let name = M::message_id();
        self.types.push(MessageType {
            name,
            stable_id: message_stable_id(name),
            version: M::VERSION,
            direction: M::DIRECTION,
            channel: M::CHANNEL,
            rate: M::RATE_LIMIT,
            always_admit: M::ALWAYS_ADMIT,
            align_to_view: M::ALIGN_TO_VIEW,
            skip_predicting_owner: M::SKIP_PREDICTING_OWNER,
            layout: h.finish(),
            decode: decode_erased::<M>,
            validate: validate_erased::<M>,
            #[cfg(feature = "ecs")]
            deliver: deliver_erased::<M>,
            #[cfg(feature = "ecs")]
            clear: clear_erased::<M>,
        });
        self.by_type.insert(TypeId::of::<M>(), index);
        self.counters.push(MessageCounters::default());
        index
    }

    /// The index of a registered type.
    pub fn index_of<M: 'static>(&self) -> Option<u16> {
        self.by_type.get(&TypeId::of::<M>()).copied()
    }

    /// A registered type by index.
    pub fn get(&self, index: u16) -> Option<&MessageType> {
        self.types.get(usize::from(index))
    }

    /// Every registered type, in registration order.
    pub fn types(&self) -> &[MessageType] {
        &self.types
    }

    /// The type with this path, for the agent protocol.
    pub fn find(&self, name: &str) -> Option<u16> {
        self.types
            .iter()
            .position(|t| t.name == name || t.name.rsplit("::").next() == Some(name))
            .map(|i| i as u16)
    }

    /// The counters of one type (mutable).
    pub fn counters_mut(&mut self, index: u16) -> Option<&mut MessageCounters> {
        self.counters.get_mut(usize::from(index))
    }

    /// The type with this stable id ([`MessageType::stable_id`]).
    pub fn find_stable(&self, stable_id: u64) -> Option<u16> {
        self.types
            .iter()
            .position(|t| t.stable_id == stable_id)
            .map(|i| i as u16)
    }

    /// Adds every type's id, version, direction, channel and layout to the protocol hash.
    pub fn hash_into(&self, h: &mut LayoutHasher) {
        h.write_str("messages");
        h.write_u64(self.types.len() as u64);
        for t in &self.types {
            h.write_str(t.name);
            h.write_u64(u64::from(t.version));
            h.write_str(t.direction.as_str());
            h.write_str(t.channel.as_str());
            h.write_u64(t.layout);
        }
    }

    /// One `(type path, fingerprint)` per registered message: the fields [`Self::hash_into`] feeds
    /// for that type (dev builds name the differing types in `protocol_mismatch`, spec 77 § 4).
    pub fn type_fingerprints(&self) -> Vec<(String, u64)> {
        self.types
            .iter()
            .map(|t| {
                let mut h = LayoutHasher::new();
                h.write_u64(u64::from(t.version));
                h.write_str(t.direction.as_str());
                h.write_str(t.channel.as_str());
                h.write_u64(t.layout);
                (t.name.to_owned(), h.finish())
            })
            .collect()
    }

    /// The per-type rates, falling back to `default` (flood guard input).
    pub fn rates(&self, default: RateLimit) -> Vec<RateLimit> {
        self.types.iter().map(|t| t.rate.unwrap_or(default)).collect()
    }

    /// Decodes one message payload of the type at `index` (a payload written by
    /// `codec::encode_to_vec`), type-erased: downcast it with `Box::downcast::<M>`. A host that
    /// carries messages in its own frames (no ECS, spec 120 § 3.2) decodes them through this. An
    /// index no type is registered at is `DecodeError::Invalid("message type")`; trailing bits
    /// after the value are ignored, as on the session path.
    pub fn decode(
        &self,
        index: u16,
        payload: &[u8],
        cx: &CodecContext,
    ) -> Result<ErasedMessage, DecodeError> {
        let t = self.get(index).ok_or(DecodeError::Invalid("message type"))?;
        (t.decode)(&mut BitReader::new(payload), cx)
    }

    /// Runs the `validate` of the type at `index` on a decoded message (`Reject("message type
    /// mismatch")` when the message is of another type, `Reject("message type")` for an unknown
    /// index).
    pub fn validate(&self, index: u16, msg: &(dyn Any + Send + Sync), cx: &ValidateContext) -> Verdict {
        match self.get(index) {
            Some(t) => (t.validate)(msg, cx),
            None => Verdict::Reject("message type"),
        }
    }
}

/// Registers net message types on an app.
#[cfg(feature = "ecs")]
pub trait NetMessageAppExt {
    /// Registers `M`. Call once per type, on every process, in the same order (it feeds the
    /// protocol hash).
    fn add_net_message<M: NetMessage>(&mut self) -> &mut Self;
}

#[cfg(feature = "ecs")]
impl NetMessageAppExt for regia_app::App {
    fn add_net_message<M: NetMessage>(&mut self) -> &mut Self {
        register_net_message::<M>(self.world_mut());
        self
    }
}

/// Registers `M` on a world (what [`NetMessageAppExt::add_net_message`] does).
#[cfg(feature = "ecs")]
pub fn register_net_message<M: NetMessage>(world: &mut World) {
    world
        .get_resource_or_insert_with(NetMessageRegistry::default)
        .register::<M>();
    world.init_resource::<NetMessages<M>>();
}

/// The system parameter that sends net messages (spec 70 § 6.2).
#[cfg(feature = "ecs")]
#[derive(SystemParam)]
pub struct NetSender<'w> {
    node: ResMut<'w, NetNode>,
    mode: Option<Res<'w, NetMode>>,
    entities: Res<'w, NetEntities>,
    registry: Res<'w, NetMessageRegistry>,
    /// The asset-server context: `Handle<T>` fields in a message travel as their guid or path,
    /// as in a replicated component (spec 71 § 3, ADR-2665 (2)).
    assets: Option<Res<'w, crate::assets::NetAssets>>,
}

#[cfg(feature = "ecs")]
impl NetSender<'_> {
    /// The tick being simulated (server) or the newest applied server tick (client).
    pub fn tick(&self) -> NetTick {
        self.node.tick
    }

    fn mode(&self) -> NetMode {
        self.mode.as_deref().copied().unwrap_or_default()
    }

    /// The connected peers.
    pub fn peers(&self) -> Vec<PeerId> {
        self.node.peers()
    }

    /// Server: charges one refusal to a connected client and names it
    /// ([`crate::runtime::NetNode::reject`]; spec 120 § 11.2, ADR-7984). For a system that
    /// decodes client bytes itself (the voice relay, a game's own blob): a fatal kind disconnects
    /// the sender, any other scores one malformed point. False when `peer` is not a connected
    /// client.
    pub fn reject_peer(
        &mut self,
        peer: PeerId,
        kind: crate::server::RejectKind,
        detail: &'static str,
    ) -> bool {
        self.node.reject(peer, kind, detail)
    }

    /// Client to server, on an entity the sender owns.
    pub fn send_to_server<M: ToServerMessage>(&mut self, target: Entity, msg: M) -> Result<(), NetSendError> {
        let id = self.target_id(target)?;
        self.queue::<M>(Direction::ToServer, Some(id), Recipient::Server, &msg)
    }

    /// Server to the owning client of `target`.
    pub fn send_to_owner<M: ToOwnerMessage>(&mut self, target: Entity, msg: M) -> Result<(), NetSendError> {
        let id = self.target_id(target)?;
        self.queue::<M>(Direction::ToOwner, Some(id), Recipient::OwnerOf(id), &msg)
    }

    /// Server to every client the target is relevant to, and to the server itself.
    pub fn broadcast<M: BroadcastMessage>(&mut self, target: Entity, msg: M) -> Result<(), NetSendError> {
        let id = self.target_id(target)?;
        self.queue::<M>(Direction::Broadcast, Some(id), Recipient::All, &msg)
    }

    /// A session message (no target) from a client to the server.
    pub fn send_session_to_server<M: ToServerMessage>(&mut self, msg: M) -> Result<(), NetSendError> {
        self.queue::<M>(Direction::ToServer, None, Recipient::Server, &msg)
    }

    /// A session message from the server to one named peer.
    pub fn send_session_to_peer<M: ToOwnerMessage>(
        &mut self,
        peer: PeerId,
        msg: M,
    ) -> Result<(), NetSendError> {
        self.queue::<M>(Direction::ToOwner, None, Recipient::Peer(peer), &msg)
    }

    /// A session message from the server to every client in game and to the server.
    pub fn broadcast_session<M: BroadcastMessage>(&mut self, msg: M) -> Result<(), NetSendError> {
        self.queue::<M>(Direction::Broadcast, None, Recipient::All, &msg)
    }

    fn target_id(&self, target: Entity) -> Result<NetId, NetSendError> {
        self.entities.net_id(target).ok_or(NetSendError::NotReplicated)
    }

    fn queue<M: NetMessage>(
        &mut self,
        direction: Direction,
        target: Option<NetId>,
        to: Recipient,
        msg: &M,
    ) -> Result<(), NetSendError> {
        if M::DIRECTION != direction {
            return Err(NetSendError::WrongDirection);
        }
        let Some(type_index) = self.registry.index_of::<M>() else {
            return Err(NetSendError::Unregistered);
        };
        let mode = self.mode();
        match direction {
            Direction::ToOwner if !mode.is_server() => {
                self.node.log_send_error_once(type_index, NetSendError::NotServer);
                return Err(NetSendError::NotServer);
            }
            Direction::ToServer if mode == NetMode::DedicatedServer => {
                self.node.log_send_error_once(type_index, NetSendError::NotClient);
                return Err(NetSendError::NotClient);
            }
            _ => {}
        }
        let asset_cx = self.assets.as_deref().and_then(crate::assets::NetAssets::context);
        let cx = CodecContext::new(&*self.entities).with_assets(asset_cx.as_deref());
        let mut w = BitWriter::new();
        msg.encode(&mut w, &cx);
        let payload = w.finish();
        if payload.len() > MAX_MESSAGE_BYTES {
            return Err(NetSendError::TooLarge);
        }
        if let Recipient::Peer(peer) = to
            && mode.is_server()
            && peer != PeerId::SERVER
            && !self.node.has_peer(peer)
        {
            return Err(NetSendError::UnknownPeer);
        }
        self.node.queue_out(OutMessage {
            type_index,
            channel: M::CHANNEL,
            direction,
            target,
            to,
            payload,
        });
        Ok(())
    }
}
