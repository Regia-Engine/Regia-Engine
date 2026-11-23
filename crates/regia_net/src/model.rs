// SPDX-License-Identifier: MPL-2.0
//! The network model (spec 70 §§ 2-5): net modes and their run conditions, peers and players,
//! network ids and ticks, authority, ownership resolution and roles.
//!
//! The rule behind all of it: gameplay written for the network runs unchanged offline. An
//! [`NetMode::Offline`] process is its own server, so the same systems, messages and ownership
//! checks run in single-player with no transport.

#[cfg(feature = "ecs")]
use std::collections::{HashMap, HashSet};

#[cfg(feature = "ecs")]
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

pub use crate::session::PeerId;

/// What this process is. A resource; it changes only through the session (spec 77), never
/// mid-tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ecs", derive(Resource, Reflect))]
#[cfg_attr(feature = "ecs", reflect(Resource, Default))]
pub enum NetMode {
    /// No networking. The process is its own server and accepts no connections.
    #[default]
    Offline,
    /// Hosts a session; no local players, no rendering, no audio (spec 76).
    DedicatedServer,
    /// Hosts a session and has local players.
    ListenServer,
    /// Connected (or connecting) to one server.
    Client,
}

impl NetMode {
    /// `Offline`, `DedicatedServer` or `ListenServer`.
    pub fn is_server(self) -> bool {
        matches!(self, Self::Offline | Self::DedicatedServer | Self::ListenServer)
    }

    /// `Client`.
    pub fn is_client(self) -> bool {
        matches!(self, Self::Client)
    }

    /// `Offline`, `ListenServer` or `Client`.
    pub fn has_local_players(self) -> bool {
        matches!(self, Self::Offline | Self::ListenServer | Self::Client)
    }

    /// `DedicatedServer` or `ListenServer`.
    pub fn accepts_connections(self) -> bool {
        matches!(self, Self::DedicatedServer | Self::ListenServer)
    }

    /// Lowercase name for logs and the agent protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::DedicatedServer => "dedicated_server",
            Self::ListenServer => "listen_server",
            Self::Client => "client",
        }
    }
}

#[cfg(feature = "ecs")]
fn mode_of(mode: Option<Res<NetMode>>) -> NetMode {
    mode.map(|m| *m).unwrap_or_default()
}

#[cfg(feature = "ecs")]
/// Run condition: this process is a server (offline, dedicated or listen).
pub fn on_server() -> impl FnMut(Option<Res<NetMode>>) -> bool + Clone {
    |mode: Option<Res<NetMode>>| mode_of(mode).is_server()
}

#[cfg(feature = "ecs")]
/// Run condition: this process is a client.
pub fn on_client() -> impl FnMut(Option<Res<NetMode>>) -> bool + Clone {
    |mode: Option<Res<NetMode>>| mode_of(mode).is_client()
}

#[cfg(feature = "ecs")]
/// Run condition: this process has local players (offline, listen server or client).
pub fn with_local_players() -> impl FnMut(Option<Res<NetMode>>) -> bool + Clone {
    |mode: Option<Res<NetMode>>| mode_of(mode).has_local_players()
}

#[cfg(feature = "ecs")]
/// Run condition: this process is offline.
pub fn offline() -> impl FnMut(Option<Res<NetMode>>) -> bool + Clone {
    |mode: Option<Res<NetMode>>| mode_of(mode) == NetMode::Offline
}

/// At most this many local players per peer (`NetPlayerId::local` 0..=7, spec 70 § 4).
pub const MAX_LOCAL_PLAYERS: u8 = 8;

/// A human participant: the peer plus a split-screen index on that peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ecs", derive(Reflect))]
pub struct NetPlayerId {
    /// The peer the player plays on.
    pub peer: PeerId,
    /// Split-screen index on that peer (0..=7).
    pub local: u8,
}

/// On a `Player` entity (`regia_framework::player`), on every process that knows the player: its
/// identity on the wire. `Player.index` stays the process-local slot.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "ecs", derive(Component, Reflect))]
#[cfg_attr(feature = "ecs", reflect(Component))]
pub struct NetPlayer {
    /// The player's network identity.
    pub id: NetPlayerId,
}

/// The id of a replicated entity (spec 71 § 4.1): assigned by the server, unique for the session,
/// never reused. A component on every replicated entity, on every process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ecs", derive(Component, Reflect))]
#[cfg_attr(feature = "ecs", reflect(Component))]
pub struct NetId(pub u32);

/// A server tick, counted from 0 at session start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ecs", derive(Reflect))]
pub struct NetTick(pub u32);

impl NetTick {
    /// The next tick (saturating: a session that reaches `u32::MAX` ends with `server_shutdown`, spec 70 § 2).
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// A fractional server tick (interpolation and lag compensation, specs 74-75).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ecs", derive(Reflect))]
pub struct NetTickFrac(pub f64);

/// The `PeerId` this process has: `Some(PeerId::SERVER)` on servers and offline, the id from the
/// welcome message on a client, `None` until then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ecs", derive(Resource))]
pub struct LocalPeer(pub Option<PeerId>);

/// Server side: which peer owns this entity. Absent = owned by the server (or by the first owner
/// found up the chain, see `resolve_owner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "ecs", derive(Component, Reflect))]
#[cfg_attr(feature = "ecs", reflect(Component))]
pub struct NetOwner(pub PeerId);

#[cfg(feature = "ecs")]
/// Server side: borrow ownership (and relevance, spec 72) from another entity that is not a
/// transform parent, for example a weapon held through an attachment socket.
#[derive(Component, Reflect, Debug, Clone, Copy)]
#[reflect(Component)]
pub struct NetOwnerLink(pub Entity);

/// Client side: this replicated copy is owned by this client. Inserted and removed by `regia_net`;
/// clients never learn other entities' owners.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "ecs", derive(Component, Reflect))]
#[cfg_attr(feature = "ecs", reflect(Component, Default))]
pub struct NetOwnedLocally;

#[cfg(feature = "ecs")]
/// Marker on a silent peer's entities. Proxy interpolation freezes while present; authoritative
/// gameplay should exclude it from input/movement until traffic restores the peer. The peer keeps
/// its slot, ownership and lifecycle phase (spec 120 § 10.2, ADR-7950).
///
/// The server always sets it on the authority copies of the peer's owned entities and player
/// controllers, and removes it when the peer is restored, leaves, or the entity changes owner.
/// Replicating it to clients is opt-in
/// ([`NetPlugin::replicate_interrupted_marker`](crate::NetPlugin::replicate_interrupted_marker)
/// on every process): a replicated component changes the protocol hash and the component indices,
/// so registering it by default would make every game refuse its v0.5.0 peers and replays.
#[cfg(feature = "ecs")]
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq, regia_macros::Replicate)]
#[reflect(Component, Default)]
pub struct NetInterrupted;

#[cfg(feature = "ecs")]
/// Server: the peers whose transport reports them interrupted now (spec 120 § 10.2, ADR-7950).
///
/// The `NetInterrupted` marker on an authority entity is a function of this set and the entity's
/// owner: `mark_peer_interrupted` keeps the set, every leave path (transport leave, kick, session
/// retire) clears its peer, and `reconcile_interrupted` strips the marker from every entity whose
/// owner is not in the set, so a marker cannot outlive the outage, the peer or a hand-off.
#[cfg(feature = "ecs")]
#[derive(Resource, Debug, Default)]
pub(crate) struct InterruptedPeers(pub(crate) HashSet<PeerId>);

#[cfg(feature = "ecs")]
/// Changes the marker on the peer's owned entities and player controllers, and records the peer
/// in (or removes it from) `InterruptedPeers`.
#[cfg(feature = "ecs")]
pub(crate) fn mark_peer_interrupted(world: &mut World, peer: PeerId, interrupted: bool) {
    let _span = tracing::trace_span!("net_mark_interrupted").entered();
    if interrupted {
        world
            .get_resource_or_insert_with(InterruptedPeers::default)
            .0
            .insert(peer);
    } else if let Some(mut peers) = world.get_resource_mut::<InterruptedPeers>() {
        peers.0.remove(&peer);
    }
    let mut query = world.query_filtered::<(Entity, Option<&NetOwner>, Option<&NetPlayer>),
        Or<(With<NetOwner>, With<NetPlayer>)>>();
    let entities: Vec<_> = query
        .iter(world)
        .filter(|(_, owner, player)| {
            owner.is_some_and(|o| o.0 == peer) || player.is_some_and(|p| p.id.peer == peer)
        })
        .map(|(e, _, _)| e)
        .collect();
    for entity in entities {
        if let Ok(mut e) = world.get_entity_mut(entity) {
            if interrupted {
                e.insert(NetInterrupted);
            } else if e.contains::<NetInterrupted>() {
                e.remove::<NetInterrupted>();
            }
        }
    }
}

#[cfg(feature = "ecs")]
/// Server, once per tick before replication is gathered: removes `NetInterrupted` from every
/// entity whose owner (its `NetOwner`, or its `NetPlayer` peer) is not in `InterruptedPeers` now.
/// An entity handed to another peer or to the server, an entity of a peer that left, and a marked
/// entity with neither component all lose the marker, whatever path changed them.
#[cfg(feature = "ecs")]
pub(crate) fn reconcile_interrupted(world: &mut World) {
    let _span = tracing::trace_span!("net_reconcile_interrupted").entered();
    // A world where no entity ever carried the marker has nothing to reconcile.
    if world.component_id::<NetInterrupted>().is_none() {
        return;
    }
    let interrupted: HashSet<PeerId> = world
        .get_resource::<InterruptedPeers>()
        .map(|p| p.0.clone())
        .unwrap_or_default();
    let mut query =
        world.query_filtered::<(Entity, Option<&NetOwner>, Option<&NetPlayer>), With<NetInterrupted>>();
    let stale: Vec<Entity> = query
        .iter(world)
        .filter(|(_, owner, player)| {
            !(owner.is_some_and(|o| interrupted.contains(&o.0))
                || player.is_some_and(|p| interrupted.contains(&p.id.peer)))
        })
        .map(|(e, _, _)| e)
        .collect();
    for entity in stale {
        if let Ok(mut e) = world.get_entity_mut(entity) {
            e.remove::<NetInterrupted>();
        }
    }
}

/// How this process treats its copy of an entity (spec 70 § 5.3). Entities without it are local.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "ecs", derive(Component, Reflect))]
#[cfg_attr(feature = "ecs", reflect(Component))]
pub enum NetRole {
    /// Not replicated: exists only in this process.
    Local,
    /// This process is the authority (the server copy, or any entity in an offline process).
    Authority,
    /// A client copy that this client owns and simulates ahead of the server (spec 73).
    Predicted,
    /// A client copy driven only by replicated state, shown through interpolation (spec 74).
    Proxy,
}

impl NetRole {
    /// Lowercase name for logs and the agent protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Authority => "authority",
            Self::Predicted => "predicted",
            Self::Proxy => "proxy",
        }
    }
}

/// Whether this process may change the entity's replicated state.
pub fn has_authority(role: &NetRole) -> bool {
    matches!(role, NetRole::Authority | NetRole::Local)
}

/// Whether this copy is driven by this process's own player: a client copy it owns (predicted,
/// or a proxy marked `NetOwnedLocally`), or an authority copy.
///
/// This signature cannot see the entity's `NetOwner`, so it answers `true` for **every**
/// authority copy, a remote player's pawn on a listen server included. On the authority use
/// [`is_locally_controlled_owned`], which takes the entity's own `NetOwner` (corr-net,
/// ADR-5580).
pub fn is_locally_controlled(role: &NetRole, owned: Option<&NetOwnedLocally>) -> bool {
    match role {
        NetRole::Predicted => true,
        NetRole::Proxy => owned.is_some(),
        // The authority copy does not carry `NetOwnedLocally` (that marker is client-only).
        // Absent means no remote owner: this process drives the copy. Present would mean a
        // client owns it, which is not this process's local player on the authority.
        NetRole::Authority => owned.is_none(),
        NetRole::Local => owned.is_some(),
    }
}

/// [`is_locally_controlled`] with the entity's own `NetOwner`: an authority copy is locally
/// controlled only when no remote peer owns it (no `NetOwner`, or `NetOwner(PeerId::SERVER)`, the
/// host's own players). Client roles answer as [`is_locally_controlled`] does. Ownership through
/// a `NetOwnerLink` or parent chain is not followed: pass the owner `resolve_owner` found for
/// such an entity.
pub fn is_locally_controlled_owned(
    role: &NetRole,
    owned: Option<&NetOwnedLocally>,
    owner: Option<&NetOwner>,
) -> bool {
    match role {
        NetRole::Authority => owner.is_none_or(|o| o.0 == PeerId::SERVER) && owned.is_none(),
        _ => is_locally_controlled(role, owned),
    }
}

/// Longest ownership chain walked before giving up (spec 70 § 5.2).
pub const MAX_OWNER_CHAIN: usize = 32;

/// The result of an ownership walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerResolution {
    /// The first `NetOwner` up the chain.
    Owned(PeerId),
    /// No `NetOwner` on the chain: owned by the server.
    Server,
    /// The walk hit a cycle or passed [`MAX_OWNER_CHAIN`] steps: treated as server-owned.
    Broken,
}

impl OwnerResolution {
    /// The owning peer (`PeerId::SERVER` for server-owned and broken chains).
    pub fn peer(self) -> PeerId {
        match self {
            Self::Owned(p) => p,
            Self::Server | Self::Broken => PeerId::SERVER,
        }
    }
}

#[cfg(all(test, feature = "ecs"))]
thread_local! {
    static OWNER_WALKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Sets the thread's `resolve_owner` count to zero.
#[cfg(all(test, feature = "ecs"))]
pub(crate) fn reset_owner_walks() {
    OWNER_WALKS.with(|c| c.set(0));
}

/// `resolve_owner` calls on this thread since [`reset_owner_walks`].
#[cfg(all(test, feature = "ecs"))]
pub(crate) fn owner_walks() -> u64 {
    OWNER_WALKS.with(|c| c.get())
}

#[cfg(feature = "ecs")]
/// Walks up from `entity`: its own `NetOwner`, else its `NetOwnerLink` target, else its `ChildOf`
/// parent, repeated. The first `NetOwner` decides.
pub fn resolve_owner(world: &World, entity: Entity) -> OwnerResolution {
    #[cfg(test)]
    OWNER_WALKS.with(|c| c.set(c.get() + 1));
    // A cycle never reaches an owner, so it runs into the step limit and reads as broken; no visited
    // set is needed (and none is allocated per walk).
    let mut current = entity;
    for _ in 0..MAX_OWNER_CHAIN {
        let Ok(e) = world.get_entity(current) else {
            return OwnerResolution::Server;
        };
        if let Some(owner) = e.get::<NetOwner>() {
            return OwnerResolution::Owned(owner.0);
        }
        if let Some(link) = e.get::<NetOwnerLink>() {
            current = link.0;
        } else if let Some(parent) = e.get::<ChildOf>() {
            current = parent.0;
        } else {
            return OwnerResolution::Server;
        }
    }
    OwnerResolution::Broken
}

#[cfg(feature = "ecs")]
/// Owners resolved once per tick (spec 70 § 11: never a walk per message). Also remembers which
/// entities already logged `net.owner_chain_broken`.
#[derive(Resource, Debug, Default)]
pub struct OwnerCache {
    owners: HashMap<Entity, PeerId>,
    warned: HashSet<Entity>,
    /// How many times `net.owner_chain_broken` was logged.
    pub broken_logged: u32,
    /// Signature of the lag-compensation owner cache. `None` until the first record.
    record_sig: Option<(u64, u64)>,
    /// Owners resolved for lag compensation. `clear` does not drop these.
    record_owners: HashMap<Entity, OwnerResolution>,
}

#[cfg(feature = "ecs")]
impl OwnerCache {
    /// The cached owner of an entity (`None` when it was not resolved this tick).
    pub fn get(&self, entity: Entity) -> Option<PeerId> {
        self.owners.get(&entity).copied()
    }

    /// The owner of `entity` in this pass: the cached owner when this pass already resolved it,
    /// else [`OwnerCache::resolve`]. The message passes use it so a burst of messages to one entity
    /// walks its chain once (spec 70 § 11, ADR-5647); they [`OwnerCache::clear`] the cache first
    /// and after any step that can change ownership.
    pub fn owner(&mut self, world: &World, entity: Entity) -> PeerId {
        match self.owners.get(&entity) {
            Some(&peer) => peer,
            None => self.resolve(world, entity),
        }
    }

    /// Resolves and caches one entity, logging a broken chain once per entity.
    pub fn resolve(&mut self, world: &World, entity: Entity) -> PeerId {
        let r = resolve_owner(world, entity);
        if r == OwnerResolution::Broken && self.warned.insert(entity) {
            self.broken_logged += 1;
            tracing::warn!(target: "net", entity = ?entity, "net.owner_chain_broken");
        }
        let peer = r.peer();
        self.owners.insert(entity, peer);
        peer
    }

    /// The lag-compensation owner of `entity`. The same `sig` returns the cached resolution and
    /// does not walk. A new `sig` drops the cache first. `clear` does not drop it.
    pub(crate) fn record_owner(&mut self, world: &World, entity: Entity, sig: (u64, u64)) -> OwnerResolution {
        if self.record_sig != Some(sig) {
            self.record_owners.clear();
            self.record_sig = Some(sig);
        } else if let Some(found) = self.record_owners.get(&entity).copied() {
            return found;
        }
        let resolved = resolve_owner(world, entity);
        self.record_owners.insert(entity, resolved);
        resolved
    }

    /// Forgets every cached owner (the next tick resolves again).
    ///
    /// The lag-compensation cache filled by `record_owner` stays: a send or apply tick clears the
    /// per-message map without forcing history recording to walk again.
    pub fn clear(&mut self) {
        self.owners.clear();
    }

    /// Forgets a despawned entity entirely.
    pub fn forget(&mut self, entity: Entity) {
        self.owners.remove(&entity);
        self.warned.remove(&entity);
        self.record_owners.remove(&entity);
    }

    /// Drops lag-compensation rows whose entities are gone, so a stable signature does not keep
    /// every hit shape that ever recorded.
    pub(crate) fn drop_recorded_gone(&mut self, world: &World) {
        self.record_owners
            .retain(|entity, _| world.get_entity(*entity).is_ok());
    }
}

#[cfg(feature = "ecs")]
/// Server side: sets or clears a character's owner when a networked player's controller possesses
/// or unpossesses it (spec 70 § 5.2). The framework's possession code calls this; `None` removes
/// the owner.
pub fn set_possession_owner(world: &mut World, character: Entity, owner: Option<PeerId>) {
    let Ok(mut e) = world.get_entity_mut(character) else {
        return;
    };
    match owner {
        Some(peer) => {
            e.insert(NetOwner(peer));
        }
        None => {
            e.remove::<NetOwner>();
        }
    }
}

#[cfg(all(test, feature = "ecs"))]
mod tests {
    use super::*;

    #[test]
    fn mode_predicates() {
        use NetMode::*;
        let table = [
            (Offline, true, false, true, false),
            (DedicatedServer, true, false, false, true),
            (ListenServer, true, false, true, true),
            (Client, false, true, true, false),
        ];
        for (m, server, client, local, accepts) in table {
            assert_eq!(m.is_server(), server, "{m:?}");
            assert_eq!(m.is_client(), client, "{m:?}");
            assert_eq!(m.has_local_players(), local, "{m:?}");
            assert_eq!(m.accepts_connections(), accepts, "{m:?}");
        }
        assert_eq!(NetMode::default(), Offline);
    }

    /// Spec 70 § 10 test 2: `on_server()` systems run offline, dedicated and listen, never on a
    /// client.
    #[test]
    fn on_server_gates_systems_by_mode() {
        #[derive(Resource, Default)]
        struct Ran(u32);
        for (mode, expect) in [
            (NetMode::Offline, 1),
            (NetMode::DedicatedServer, 1),
            (NetMode::ListenServer, 1),
            (NetMode::Client, 0),
        ] {
            let mut world = World::new();
            world.insert_resource(mode);
            world.init_resource::<Ran>();
            let mut schedule = Schedule::default();
            schedule.add_systems((|mut r: ResMut<Ran>| r.0 += 1).run_if(on_server()));
            schedule.add_systems((|mut r: ResMut<Ran>| r.0 += 10).run_if(on_client()));
            schedule.run(&mut world);
            let ran = world.resource::<Ran>().0;
            assert_eq!(ran % 10, expect, "{mode:?}");
            assert_eq!(ran / 10, u32::from(mode == NetMode::Client), "{mode:?}");
        }
    }

    /// Spec 70 § 3 (lane spec-28-68): `with_local_players()` runs on offline, listen and client
    /// processes and never on a dedicated server; `offline()` only offline. With no `NetMode`
    /// resource the process is `Offline` (the default).
    #[test]
    fn local_player_and_offline_conditions_gate_systems_by_mode() {
        #[derive(Resource, Default)]
        struct Ran(u32);
        for (mode, players, off) in [
            (Some(NetMode::Offline), 1, 1),
            (Some(NetMode::DedicatedServer), 0, 0),
            (Some(NetMode::ListenServer), 1, 0),
            (Some(NetMode::Client), 1, 0),
            (None, 1, 1),
        ] {
            let mut world = World::new();
            if let Some(m) = mode {
                world.insert_resource(m);
            }
            world.init_resource::<Ran>();
            let mut schedule = Schedule::default();
            schedule.add_systems((|mut r: ResMut<Ran>| r.0 += 1).run_if(with_local_players()));
            schedule.add_systems((|mut r: ResMut<Ran>| r.0 += 10).run_if(offline()));
            schedule.run(&mut world);
            let ran = world.resource::<Ran>().0;
            assert_eq!(ran % 10, players, "{mode:?}: with_local_players");
            assert_eq!(ran / 10, off, "{mode:?}: offline");
        }
    }

    /// Spec 70 § 10 test 3: owner through `NetOwner`, `NetOwnerLink`, two `ChildOf` levels; a
    /// 3-cycle and a 40-deep chain resolve to the server and log once.
    #[test]
    fn ownership_resolution_walks_links_and_parents() {
        let mut world = World::new();
        let pawn = world.spawn(NetOwner(PeerId(3))).id();
        let weapon = world.spawn(NetOwnerLink(pawn)).id();
        let child = world.spawn(ChildOf(pawn)).id();
        let grandchild = world.spawn(ChildOf(child)).id();
        let scope = world.spawn(ChildOf(weapon)).id();
        assert_eq!(resolve_owner(&world, pawn), OwnerResolution::Owned(PeerId(3)));
        assert_eq!(resolve_owner(&world, weapon), OwnerResolution::Owned(PeerId(3)));
        assert_eq!(
            resolve_owner(&world, grandchild),
            OwnerResolution::Owned(PeerId(3))
        );
        assert_eq!(resolve_owner(&world, scope), OwnerResolution::Owned(PeerId(3)));
        let loose = world.spawn_empty().id();
        assert_eq!(resolve_owner(&world, loose), OwnerResolution::Server);

        // A 3-cycle through links.
        let a = world.spawn_empty().id();
        let b = world.spawn(NetOwnerLink(a)).id();
        let c = world.spawn(NetOwnerLink(b)).id();
        world.entity_mut(a).insert(NetOwnerLink(c));
        // A 40-deep chain of links ending at an owner.
        let mut tail = world.spawn(NetOwner(PeerId(9))).id();
        for _ in 0..40 {
            tail = world.spawn(NetOwnerLink(tail)).id();
        }
        let mut cache = OwnerCache::default();
        for _ in 0..3 {
            assert_eq!(cache.resolve(&world, a), PeerId::SERVER);
            assert_eq!(cache.resolve(&world, tail), PeerId::SERVER);
            cache.clear();
        }
        assert_eq!(cache.broken_logged, 2, "logged once per entity");
        assert_eq!(resolve_owner(&world, tail), OwnerResolution::Broken);
    }

    #[test]
    fn roles_and_helpers() {
        assert!(has_authority(&NetRole::Authority));
        assert!(!has_authority(&NetRole::Proxy));
        assert!(is_locally_controlled(&NetRole::Predicted, None));
        assert!(!is_locally_controlled(&NetRole::Proxy, None));
        assert!(is_locally_controlled(&NetRole::Proxy, Some(&NetOwnedLocally)));
        assert!(is_locally_controlled(&NetRole::Authority, None));
        assert!(!is_locally_controlled(
            &NetRole::Authority,
            Some(&NetOwnedLocally)
        ));
        assert!(!is_locally_controlled(&NetRole::Local, None));
        assert!(is_locally_controlled(&NetRole::Local, Some(&NetOwnedLocally)));
        // corr-net (ADR-5580): on the authority a remote player's pawn is not locally
        // controlled; the host's own (no owner, or the server peer) is.
        let remote = NetOwner(PeerId(3));
        let host = NetOwner(PeerId::SERVER);
        assert!(!is_locally_controlled_owned(
            &NetRole::Authority,
            None,
            Some(&remote)
        ));
        assert!(is_locally_controlled_owned(
            &NetRole::Authority,
            None,
            Some(&host)
        ));
        assert!(is_locally_controlled_owned(&NetRole::Authority, None, None));
        assert!(is_locally_controlled_owned(
            &NetRole::Predicted,
            None,
            Some(&remote)
        ));
        assert!(!is_locally_controlled_owned(&NetRole::Proxy, None, Some(&remote)));
        let mut world = World::new();
        let e = world.spawn_empty().id();
        set_possession_owner(&mut world, e, Some(PeerId(2)));
        assert_eq!(world.get::<NetOwner>(e), Some(&NetOwner(PeerId(2))));
        set_possession_owner(&mut world, e, None);
        assert!(world.get::<NetOwner>(e).is_none());
    }

    /// Review row 9 (ADR-7950): the marker follows the current owner. A hand-off and an orphaned
    /// marker are reconciled away; a leave clears the peer and every entity it marked.
    #[test]
    fn interrupted_marker_follows_the_owner_and_leaves_with_the_peer() {
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        let (a, b) = (PeerId(1), PeerId(2));
        let pawn = world.spawn(NetOwner(a)).id();
        let handed = world.spawn(NetOwner(a)).id();
        let to_server = world.spawn(NetOwner(a)).id();
        let player = world
            .spawn(NetPlayer {
                id: NetPlayerId { peer: a, local: 0 },
            })
            .id();
        let healthy = world.spawn(NetOwner(b)).id();
        let orphan = world.spawn(NetInterrupted).id();
        mark_peer_interrupted(&mut world, a, true);
        assert!(world.get::<NetInterrupted>(handed).is_some());
        assert!(world.get::<NetInterrupted>(healthy).is_none());
        world.entity_mut(handed).insert(NetOwner(b));
        world.entity_mut(to_server).remove::<NetOwner>();
        reconcile_interrupted(&mut world);
        for kept in [pawn, player] {
            assert!(world.get::<NetInterrupted>(kept).is_some());
        }
        for cleared in [handed, to_server, orphan, healthy] {
            assert!(world.get::<NetInterrupted>(cleared).is_none());
        }
        crate::runtime::server_remove_peer(&mut world, a);
        assert!(world.get::<NetInterrupted>(pawn).is_none());
        assert!(world.get::<NetInterrupted>(player).is_none());
        assert!(world.resource::<InterruptedPeers>().0.is_empty());
    }
}
