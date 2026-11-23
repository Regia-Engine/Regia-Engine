// SPDX-License-Identifier: MPL-2.0
//! Resume for replication games (spec 120 § 8.5 and § 10.3, ADR-8298).
//!
//! A player whose transport connection times out is kept **dormant** for the grace window
//! ([`crate::server::ServerConfig::resume_grace_ms`], 60 s on a [`crate::link::NetLink`]): the
//! node keeps its `PeerId`, its entities and their ownership, and its interest state; its
//! per-connection baselines are dropped; its reliable game messages wait (those older than the
//! grace are dropped when it comes back) and unreliable ones are dropped. Its owned entities carry
//! [`NetInterrupted`], which replicates, so other clients see the
//! pawn frozen.
//!
//! The client comes back with the resume ticket of its last accept
//! ([`crate::handshake::Accept::resume_ticket`]): the server binds the new connection to the same
//! `PeerId`, and the first frame it writes for it is the full state (every relevant entity as a
//! spawn record, as for a join). When the grace ends first, the game's [`NetGraceHook`] decides
//! whether the peer's entities are despawned or kept; without a hook nothing is changed and the
//! game sees the leave as before.
//!
//! The server bridge ([`crate::bridge::ServerBridge`]) calls these on the transport's
//! `PeerDormant`, `PeerResumed` and `PeerLeft` events.

use std::collections::BTreeMap;

use regia_ecs::prelude::{Entity, Resource, World, bevy_ecs};

use crate::model::{NetInterrupted, NetOwner, NetPlayer, PeerId};
use crate::replication::ServerReplication;
use crate::settings::{NetSettings, StateChannel};

/// What happens to a dormant peer's entities when its grace ends without a resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraceVerdict {
    /// Despawn every entity the peer owns (and its `NetPlayer` entities).
    Despawn,
    /// Keep them: the server takes their ownership over and the `NetInterrupted` marker goes.
    Keep,
}

impl GraceVerdict {
    /// Lowercase code (agent protocol, logs).
    pub fn code(self) -> &'static str {
        match self {
            Self::Despawn => "despawn",
            Self::Keep => "keep",
        }
    }
}

/// The game's decision at a dormant peer's grace end (spec 120 § 10.3): runs once per expired
/// peer, before the node forgets it, with its entities still in the world.
pub type GraceHook = fn(&mut World, PeerId) -> GraceVerdict;

/// The `on_grace_expired` hook of a replication game (spec 120 § 10.3). Insert it in the server
/// app; without it a grace end changes nothing in the world and the game reacts to the leave
/// event itself (the behavior before resume existed).
#[derive(Resource, Debug, Clone, Copy)]
pub struct NetGraceHook {
    /// The hook.
    pub on_grace_expired: GraceHook,
}

impl NetGraceHook {
    /// A hook resource.
    pub fn new(on_grace_expired: GraceHook) -> Self {
        Self { on_grace_expired }
    }
}

/// One dormant peer, as the node sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DormantPeer {
    /// The node tick it went dormant at.
    pub since_tick: u32,
}

/// Counters and state of replication-game resume on a server (spec 120 § 10.3). A resource the
/// bridge keeps up to date; `net.resume` reports it.
#[derive(Resource, Debug, Clone, Default)]
pub struct NetResumeState {
    /// Peers dormant now.
    pub dormant: BTreeMap<PeerId, DormantPeer>,
    /// Peers that went dormant.
    pub went_dormant: u64,
    /// Peers that came back from dormancy.
    pub resumed: u64,
    /// Resumes that took over a connection the node still held (live-token takeover).
    pub takeovers: u64,
    /// Grace ends (each one ran the hook once when a hook was set).
    pub expired: u64,
    /// Hook calls.
    pub hook_calls: u64,
    /// Grace ends the hook answered with [`GraceVerdict::Keep`].
    pub kept: u64,
    /// Entities despawned by [`GraceVerdict::Despawn`].
    pub despawned_entities: u64,
    /// Held reliable messages delivered after a resume.
    pub messages_kept: u64,
    /// Held reliable messages dropped at a resume for being older than the grace.
    pub messages_expired: u64,
    /// Held reliable messages dropped while dormant for passing the queue bound.
    pub messages_overflowed: u64,
    /// The newest resume's peer and the node tick it came back at.
    pub last_resume: Option<(PeerId, u32)>,
}

fn state(world: &mut World) -> regia_ecs::prelude::Mut<'_, NetResumeState> {
    if !world.contains_resource::<NetResumeState>() {
        world.init_resource::<NetResumeState>();
    }
    world.resource_mut::<NetResumeState>()
}

/// The entities `peer` owns or plays.
fn peer_entities(world: &mut World, peer: PeerId) -> Vec<Entity> {
    let mut q = world.query::<(Entity, Option<&NetOwner>, Option<&NetPlayer>)>();
    q.iter(world)
        .filter(|(_, o, p)| o.is_some_and(|o| o.0 == peer) || p.is_some_and(|p| p.id.peer == peer))
        .map(|(e, _, _)| e)
        .collect()
}

/// Server: `peer`'s transport connection timed out and the transport holds it for a resume.
/// Keeps its entities, ownership and interest state; drops its baselines; marks its entities
/// [`NetInterrupted`]. False when the node does not know the peer (nothing is done).
pub fn server_peer_dormant(world: &mut World, peer: PeerId) -> bool {
    let _span = tracing::debug_span!("net_peer_dormant").entered();
    if !crate::runtime::node_hold_peer(world, peer) {
        return false;
    }
    // Baselines go: the per-connection copies, acks and state link. The resume's first gather is a
    // join gather (full state).
    if let Some(mut rep) = world.get_resource_mut::<ServerReplication>() {
        rep.make_dormant(peer);
    }
    crate::model::mark_peer_interrupted(world, peer, true);
    let tick = world.resource::<crate::runtime::NetNode>().tick.0;
    let mut st = state(world);
    st.dormant.insert(peer, DormantPeer { since_tick: tick });
    st.went_dormant += 1;
    tracing::info!(target: "net", peer = peer.0, "net peer dormant: entities kept for a resume");
    true
}

/// Server: `peer` is back on a new connection with its resume ticket (from dormancy, or a
/// takeover of a connection the node still held). The first frame written for it is the full
/// state; held reliable messages younger than `grace_ms` go out again. Returns whether it was
/// dormant.
pub fn server_peer_resumed(world: &mut World, peer: PeerId, grace_ms: u64) -> bool {
    let _span = tracing::debug_span!("net_peer_resumed").entered();
    if !crate::runtime::node_knows_peer(world, peer) {
        // The node never knew it (it went dormant before this node ran): a plain join.
        crate::runtime::server_add_peer(world, peer);
        return false;
    }
    let hz = crate::runtime::tick_hz(world);
    let grace_ticks = ((grace_ms as f64 / 1000.0) * hz).ceil().min(f64::from(u32::MAX)) as u32;
    let held = crate::runtime::node_resume_peer(world, peer, grace_ticks);
    let wire = match world.resource::<NetSettings>().replication.state_channel {
        StateChannel::Unreliable => crate::replication::ConnWire::Undecided,
        StateChannel::Reliable => crate::replication::ConnWire::Stream,
    };
    if let Some(mut rep) = world.get_resource_mut::<ServerReplication>() {
        // A fresh connection state: nothing is assumed held, so the next gather sends every
        // relevant entity in full (spec 120 § 8.5).
        rep.resume_conn(peer, wire);
    }
    crate::model::mark_peer_interrupted(world, peer, false);
    let tick = world.resource::<crate::runtime::NetNode>().tick.0;
    let mut st = state(world);
    st.dormant.remove(&peer);
    if held.was_dormant {
        st.resumed += 1;
    } else {
        st.takeovers += 1;
    }
    st.messages_kept += held.kept;
    st.messages_expired += held.expired;
    st.messages_overflowed += held.overflowed;
    st.last_resume = Some((peer, tick));
    tracing::info!(
        target: "net",
        peer = peer.0,
        dormant = held.was_dormant,
        kept = held.kept,
        expired = held.expired,
        "net peer resumed"
    );
    held.was_dormant
}

/// Server: a dormant peer's grace ended. Runs the game's [`NetGraceHook`] once (when set) and
/// applies its verdict, then the node forgets the peer. Returns the verdict (`None` without a
/// hook).
pub fn server_grace_expired(world: &mut World, peer: PeerId) -> Option<GraceVerdict> {
    let _span = tracing::debug_span!("net_grace_expired").entered();
    let hook = world.get_resource::<NetGraceHook>().map(|h| h.on_grace_expired);
    let verdict = hook.map(|hook| hook(world, peer));
    {
        let mut st = state(world);
        st.dormant.remove(&peer);
        st.expired += 1;
        if verdict.is_some() {
            st.hook_calls += 1;
        }
    }
    match verdict {
        Some(GraceVerdict::Despawn) => {
            let gone = peer_entities(world, peer);
            let n = gone.len() as u64;
            for e in gone {
                if let Ok(ent) = world.get_entity_mut(e) {
                    ent.despawn();
                }
            }
            state(world).despawned_entities += n;
        }
        Some(GraceVerdict::Keep) => {
            for e in peer_entities(world, peer) {
                if let Ok(mut ent) = world.get_entity_mut(e) {
                    ent.remove::<(NetOwner, NetInterrupted)>();
                }
            }
            state(world).kept += 1;
        }
        None => {}
    }
    tracing::info!(
        target: "net",
        peer = peer.0,
        verdict = verdict.map_or("none", GraceVerdict::code),
        "net dormant peer's grace ended"
    );
    crate::runtime::server_remove_peer(world, peer);
    verdict
}
