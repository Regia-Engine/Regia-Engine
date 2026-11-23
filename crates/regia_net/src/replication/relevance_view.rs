// SPDX-License-Identifier: MPL-2.0
//! What the `net_relevance` gizmo group shows (spec 72 § 10, ADR-2665 (5)): for one connection,
//! each replicated entity's relevance state, the viewpoints, the interest grid's cell size and
//! the project default profile's radius. Pure reads of the server world: the drawing lives in
//! `regia::net_relevance_bridge` (`regia_net` has no edge to the renderer) and the agent's
//! `net.relevance_view` returns the same rows.

use glam::Vec3;
use regia_ecs::prelude::*;

use super::ServerReplication;
use crate::model::{NetId, PeerId};
use crate::profile::{NetProfiles, RelevanceShape};
use crate::relevance::NetInterest;
use crate::runtime::NetNode;

/// Which connection the `net_relevance` gizmo group draws. `None` (the default): the connected
/// peer with the lowest id.
///
/// Equality is the chosen peer. The scratch snapshot is not part of it.
#[derive(Resource, Debug, Clone, Default)]
pub struct NetRelevanceView {
    /// The chosen peer.
    pub peer: Option<PeerId>,
    /// The last rebuilt view. Reused while the entity set and the interest inputs stay put.
    scratch: Option<CachedRelevance>,
    /// How many times `scratch` was rebuilt.
    fills: u64,
}

impl PartialEq for NetRelevanceView {
    fn eq(&self, other: &Self) -> bool {
        self.peer == other.peer
    }
}

impl Eq for NetRelevanceView {}

impl NetRelevanceView {
    /// How many times the scratch snapshot was rebuilt.
    pub fn snapshot_fills(&self) -> u64 {
        self.fills
    }
}

/// One cached `net_relevance` view.
#[derive(Debug, Clone)]
struct CachedRelevance {
    peer: PeerId,
    entity_gen: u64,
    interest_gen: u64,
    profile_gen: Option<u64>,
    pipeline: bool,
    snapshot: RelevanceSnapshot,
}

/// One replicated entity's state for the chosen connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelevanceMarkState {
    /// Relevant: its state is sent to the connection.
    Relevant,
    /// No longer relevant but still held for its linger time.
    Lingering,
    /// Not relevant to the connection (out of range, filtered or not visible to it).
    FilteredOut,
}

impl RelevanceMarkState {
    /// The lowercase name the agent reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Relevant => "relevant",
            Self::Lingering => "lingering",
            Self::FilteredOut => "filtered_out",
        }
    }
}

/// One replicated entity in a [`RelevanceSnapshot`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RelevanceMark {
    /// The server entity.
    pub entity: Entity,
    /// Its net id.
    pub net_id: NetId,
    /// Its state for the connection.
    pub state: RelevanceMarkState,
    /// Asleep (`NetAsleep`): drawn outlined.
    pub asleep: bool,
}

/// The `net_relevance` view of one connection.
#[derive(Debug, Clone, PartialEq)]
pub struct RelevanceSnapshot {
    /// The connection.
    pub peer: PeerId,
    /// Whether the relevance pipeline is installed (without it every entity is relevant).
    pub pipeline: bool,
    /// The interest grid's cell size in metres (0 without a pipeline).
    pub cell_m: f32,
    /// The project default profile's spatial radius (`None` for `World` relevance).
    pub radius_m: Option<f32>,
    /// The connection's viewpoints this tick: position and forward.
    pub viewpoints: Vec<(Vec3, Vec3)>,
    /// Every replicated entity, in net-id order.
    pub marks: Vec<RelevanceMark>,
}

impl RelevanceSnapshot {
    /// How many marks have `state`.
    pub fn count(&self, state: RelevanceMarkState) -> usize {
        self.marks.iter().filter(|m| m.state == state).count()
    }
}

/// The connection the view shows: [`NetRelevanceView::peer`] while it is connected, else the
/// connected peer with the lowest id. `None` with no peer (or no net runtime).
pub fn view_peer(world: &World) -> Option<PeerId> {
    view_peer_of(
        world.get_resource::<NetNode>(),
        world.get_resource::<NetRelevanceView>(),
    )
}

/// [`view_peer`] from the resources (for systems that cannot take the whole world).
pub fn view_peer_of(node: Option<&NetNode>, view: Option<&NetRelevanceView>) -> Option<PeerId> {
    let node = node?;
    match view.and_then(|v| v.peer) {
        Some(p) if node.has_peer(p) => Some(p),
        _ => node.min_peer(),
    }
}

/// The view of `peer` on a server world. `None` when the world does not replicate (no
/// `ServerReplication`).
pub fn relevance_snapshot(world: &World, peer: PeerId) -> Option<RelevanceSnapshot> {
    Some(relevance_snapshot_of(
        world.get_resource::<ServerReplication>()?,
        world.get_resource::<NetInterest>(),
        world.get_resource::<NetProfiles>(),
        peer,
        |e| world.get::<super::NetAsleep>(e).is_some(),
    ))
}

/// [`relevance_snapshot`] from the resources; `asleep` says whether an entity carries
/// `NetAsleep` (the replication state's own flag counts too).
pub fn relevance_snapshot_of(
    rep: &ServerReplication,
    interest: Option<&NetInterest>,
    profiles: Option<&NetProfiles>,
    peer: PeerId,
    asleep: impl Fn(Entity) -> bool,
) -> RelevanceSnapshot {
    let pi = interest.and_then(|i| i.peers.get(&peer));
    let mut marks: Vec<RelevanceMark> = rep
        .entities()
        .map(|(net_id, entity, sleeping, _)| {
            let state = match (interest, pi) {
                // No pipeline: every replicated entity is relevant to everyone.
                (None, _) => RelevanceMarkState::Relevant,
                (Some(_), Some(pi)) if pi.is_lingering(net_id) => RelevanceMarkState::Lingering,
                (Some(_), Some(pi)) if pi.holds(net_id) => RelevanceMarkState::Relevant,
                (Some(_), _) => RelevanceMarkState::FilteredOut,
            };
            RelevanceMark {
                entity,
                net_id,
                state,
                asleep: sleeping || asleep(entity),
            }
        })
        .collect();
    marks.sort_by_key(|m| m.net_id);
    let radius_m = profiles.and_then(|p| {
        let profile = p.get(p.default_key())?;
        match profile.relevance {
            RelevanceShape::Spatial { radius, .. } => Some(radius),
            RelevanceShape::World => None,
        }
    });
    RelevanceSnapshot {
        peer,
        pipeline: interest.is_some(),
        cell_m: interest.map_or(0.0, |i| i.grid.cell_m),
        radius_m,
        viewpoints: pi.map(|p| p.view.viewpoints.clone()).unwrap_or_default(),
        marks,
    }
}

/// Rebuilds the scratch snapshot when the replicated set or the interest inputs changed, then
/// hands it to `use_snap`. A hit still refreshes each mark's asleep bit and does not allocate
/// the mark list again.
pub fn with_cached_relevance_snapshot<R>(
    view: &mut NetRelevanceView,
    rep: &ServerReplication,
    interest: Option<&NetInterest>,
    profiles: Option<&NetProfiles>,
    peer: PeerId,
    asleep: impl Fn(Entity) -> bool,
    use_snap: impl FnOnce(&RelevanceSnapshot) -> R,
) -> R {
    let entity_gen = rep.entity_generation();
    let interest_gen = interest.map_or(0, NetInterest::generation);
    let profile_gen = profiles.map(NetProfiles::generation);
    let pipeline = interest.is_some();
    let hit = view.scratch.as_ref().is_some_and(|cached| {
        cached.peer == peer
            && cached.entity_gen == entity_gen
            && cached.interest_gen == interest_gen
            && cached.profile_gen == profile_gen
            && cached.pipeline == pipeline
    });
    if hit && let Some(cached) = view.scratch.as_mut() {
        for mark in &mut cached.snapshot.marks {
            let sleeping = rep.is_asleep(mark.net_id).unwrap_or_default();
            mark.asleep = sleeping || asleep(mark.entity);
        }
    } else {
        let snapshot = relevance_snapshot_of(rep, interest, profiles, peer, &asleep);
        view.fills = view.fills.wrapping_add(1);
        view.scratch = Some(CachedRelevance {
            peer,
            entity_gen,
            interest_gen,
            profile_gen,
            pipeline,
            snapshot,
        });
    }
    match view.scratch.as_ref() {
        Some(cached) => use_snap(&cached.snapshot),
        None => {
            let snapshot = relevance_snapshot_of(rep, interest, profiles, peer, asleep);
            use_snap(&snapshot)
        }
    }
}

/// The `net_relevance` rows for `peer`, rebuilt only when [`ServerReplication::entity_generation`],
/// [`NetInterest::generation`] or the profile generation changed since the last fill.
///
/// The gizmo system borrows the scratch and does not clone the mark list. This function clones
/// the stored snapshot for the caller. `None` when the world does not replicate.
pub fn cached_relevance_snapshot(world: &mut World, peer: PeerId) -> Option<RelevanceSnapshot> {
    world.get_resource::<ServerReplication>()?;
    if world.get_resource::<NetRelevanceView>().is_none() {
        world.insert_resource(NetRelevanceView::default());
    }
    let mut asleep_query = world.query_filtered::<Entity, With<super::NetAsleep>>();
    let asleep: Vec<Entity> = asleep_query.iter(world).collect();
    world.resource_scope(|world, mut view: Mut<NetRelevanceView>| {
        let rep = world.get_resource::<ServerReplication>()?;
        let interest = world.get_resource::<NetInterest>();
        let profiles = world.get_resource::<NetProfiles>();
        let mut cloned = None;
        with_cached_relevance_snapshot(
            &mut view,
            rep,
            interest,
            profiles,
            peer,
            |entity| asleep.contains(&entity),
            |snapshot| cloned = Some(snapshot.clone()),
        );
        cloned
    })
}
