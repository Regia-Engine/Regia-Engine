// SPDX-License-Identifier: MPL-2.0
//! Relevance (spec 72 § 5): which replicated entities each connection holds.
//!
//! Opt-in: the pipeline runs when the server world has a [`NetInterest`] resource
//! ([`install_net_interest`]); without it every replicated entity is relevant to every connection
//! (spec 71 behavior, plus the legacy `NetRelevance` filter seam). Each send tick the server
//! evaluates, for each connection, only the candidates the shared [`InterestGrid`] and the
//! per-tick lists give it (owned, world-relevant, granted, link-followed and currently held
//! entities); no per-entity, per-connection loop runs over the whole world.
//!
//! Precedence, highest first (spec 72 § 5.1): owner rule, denies (peer and group),
//! owner-exclusive, grants (`sent_to_all`, link rule, temporary grants), then the filters (allow
//! list, `group_required`, spatial interest with hysteresis, custom filter). Losing relevance
//! starts the profile's `linger_ticks`; regaining it during the linger costs nothing.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use glam::Vec3;
use regia_ecs::prelude::*;
use regia_scene::transform::{GlobalTransform, Transform};

use crate::dense::{HeldSlots, SlotBits};
pub use crate::interest::InterestGrid;
use crate::interest::{GridAt, spatial_limit};
use crate::model::{NetId, NetOwnerLink, NetPlayer, NetPlayerId, NetTick, PeerId};
use crate::profile::{NetProfile, NetProfileRef, NetProfiles, RelevanceShape};
use crate::replication::NetGroups;
use crate::send::{ConnSendState, EntitySendState};

/// An explicit allow list (non-empty = only those peers) and a deny list, on a replicated entity.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub struct NetVisibility {
    /// If non-empty, only these peers may hold the entity.
    pub allow: Vec<PeerId>,
    /// These peers never hold the entity (unless they own it).
    pub deny: Vec<PeerId>,
}

/// Group visibility of a connection: on its `NetPlayer` entities (the union for split-screen
/// peers). An entity with a denied group is not relevant; with `group_required` it needs an
/// allowed one.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub struct NetGroupVisibility {
    /// Groups this connection may see.
    pub allow: Vec<String>,
    /// Groups this connection never sees.
    pub deny: Vec<String>,
}

/// A viewpoint of a player (on a camera or character entity with a `Transform`). Forward is the
/// entity's local −Z. A player without one uses its controlled character: replicated entities it
/// owns that are predicted (spec 70 § 5.3).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetViewpoint {
    /// The player this viewpoint belongs to.
    pub player: NetPlayerId,
}

/// A custom relevance filter (spec 72 § 5.1 stage 6). It can only remove entities.
pub trait NetRelevanceFilter: Send + Sync + 'static {
    /// Whether `entity` may be relevant to `peer`.
    fn relevant(world: &World, entity: Entity, peer: PeerId) -> bool;
}

/// A registered filter function.
pub type RelevanceFilterFn = fn(&World, Entity, PeerId) -> bool;

/// The registered custom filters, by name.
#[derive(Resource, Debug, Default, Clone)]
pub struct NetRelevanceFilters {
    filters: BTreeMap<String, RelevanceFilterFn>,
    /// Names registered twice (a startup error; the first registration stays).
    pub duplicates: Vec<String>,
}

impl NetRelevanceFilters {
    /// Registers a filter. A second registration of a name is refused and logged
    /// `net.filter_duplicate` at error (spec 72: a startup error).
    pub fn register(&mut self, name: &str, f: RelevanceFilterFn) -> bool {
        if self.filters.contains_key(name) {
            tracing::error!(target: "net", filter = name, "net.filter_duplicate");
            self.duplicates.push(name.to_string());
            return false;
        }
        self.filters.insert(name.to_string(), f);
        true
    }

    /// A filter by name.
    pub fn get(&self, name: &str) -> Option<RelevanceFilterFn> {
        self.filters.get(name).copied()
    }

    /// Registered names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.filters.keys().map(String::as_str)
    }
}

/// Registers a custom relevance filter on a world (`app.add_net_relevance_filter::<F>(name)`).
pub fn add_net_relevance_filter<F: NetRelevanceFilter>(world: &mut World, name: &str) -> bool {
    world.init_resource::<NetRelevanceFilters>();
    world
        .resource_mut::<NetRelevanceFilters>()
        .register(name, F::relevant)
}

/// `app.add_net_relevance_filter::<F>("name")`.
pub trait NetRelevanceAppExt {
    /// Registers a custom relevance filter under a name profiles can use.
    fn add_net_relevance_filter<F: NetRelevanceFilter>(&mut self, name: &str) -> &mut Self;
}

impl NetRelevanceAppExt for regia_app::App {
    fn add_net_relevance_filter<F: NetRelevanceFilter>(&mut self, name: &str) -> &mut Self {
        add_net_relevance_filter::<F>(self.world_mut(), name);
        self
    }
}

/// Who a temporary grant makes the entity relevant to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GrantTarget {
    /// One peer.
    Peer(PeerId),
    /// Every peer whose players carry this group in `NetGroups`.
    Group(String),
}

/// A temporary relevance grant (spec 72 § 5.1 stage 7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelevanceGrant {
    /// The granted entity.
    pub entity: Entity,
    /// Who sees it.
    pub target: GrantTarget,
    /// The grant holds while the server tick is below this.
    pub until: NetTick,
}

/// The stage of § 5.1 that decided an entity's relevance (`net.why_not_relevant`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelevanceStage {
    /// Owned by the connection's peer.
    Owner,
    /// `sent_to_all`.
    SentToAll,
    /// `owner_only` (and not the owner).
    OwnerOnly,
    /// `NetVisibility` allow or deny.
    Visibility,
    /// Group visibility (deny, or `group_required`).
    Group,
    /// Link rule (`follow_parent`).
    Link,
    /// Spatial interest (or world relevance passing it).
    Spatial,
    /// A custom filter.
    Custom,
    /// A temporary grant.
    Grant,
}

impl RelevanceStage {
    /// Every stage, by the index stored per (connection, entity).
    const ALL: [Self; 9] = [
        Self::Owner,
        Self::SentToAll,
        Self::OwnerOnly,
        Self::Visibility,
        Self::Group,
        Self::Link,
        Self::Spatial,
        Self::Custom,
        Self::Grant,
    ];

    fn index(self) -> u8 {
        match self {
            Self::Owner => 0,
            Self::SentToAll => 1,
            Self::OwnerOnly => 2,
            Self::Visibility => 3,
            Self::Group => 4,
            Self::Link => 5,
            Self::Spatial => 6,
            Self::Custom => 7,
            Self::Grant => 8,
        }
    }

    /// The protocol name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::SentToAll => "sent_to_all",
            Self::OwnerOnly => "owner_only",
            Self::Visibility => "visibility",
            Self::Group => "group",
            Self::Link => "link",
            Self::Spatial => "spatial",
            Self::Custom => "custom",
            Self::Grant => "grant",
        }
    }
}

/// The result of evaluating one (connection, entity).
#[derive(Debug, Clone, PartialEq)]
pub struct RelevanceVerdict {
    /// Relevant by the pipeline (before linger).
    pub relevant: bool,
    /// The deciding stage.
    pub stage: RelevanceStage,
    /// A human-readable detail.
    pub detail: String,
    /// Distance to the nearest viewpoint (`None` without viewpoints or position).
    pub distance: Option<f32>,
    /// Inside some viewpoint's 120° forward cone.
    pub in_view: bool,
    /// The spatial test passed (hysteresis state for the next tick).
    pub spatial_in: bool,
}

/// One connection's view of the world for a tick.
#[derive(Debug, Clone, Default)]
pub struct PeerView {
    /// Viewpoints: position and forward.
    pub viewpoints: Vec<(Vec3, Vec3)>,
    /// Groups allowed by `NetGroupVisibility`.
    pub group_allow: HashSet<String>,
    /// Groups denied by `NetGroupVisibility`.
    pub group_deny: HashSet<String>,
    /// Groups the peer's players carry (`NetGroups`; for group grants).
    pub groups: HashSet<String>,
    /// The peer's session has not reached `InGame` ([`NetSessionPeers`]): `sent_to_all`
    /// entities wait (spec 72 § 5.1, ADR-1662 (8), ADR-7213). `false` (the default) for a peer
    /// the session does not know.
    pub not_in_game: bool,
}

/// Server: what the session layer (spec 77) knows about each replication connection, written by
/// `net_session_system` every update (ADR-7213). A game without a `NetSession`, or a peer the
/// session does not know (a link-only peer, a virtual peer), has no row: such a peer counts as
/// in game with no declared rate.
#[derive(Resource, Debug, Clone, Default)]
pub struct NetSessionPeers {
    /// One row per session peer.
    pub peers: HashMap<PeerId, SessionPeer>,
}

/// One session peer as replication sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPeer {
    /// The peer's session state is `InGame` (spec 77 § 3).
    pub in_game: bool,
    /// The rate the client declared in its `Hello` (`client_rate_bytes`), `None` for "the
    /// server's default". It lowers the peer's replication budget, never raises it (spec 72 § 7).
    pub declared_rate_bytes: Option<u32>,
}

impl NetSessionPeers {
    /// Whether `peer` is in game (true for a peer without a row).
    pub fn in_game(&self, peer: PeerId) -> bool {
        self.peers.get(&peer).is_none_or(|p| p.in_game)
    }

    /// The rate `peer` declared, if any.
    pub fn declared_rate(&self, peer: PeerId) -> Option<u32> {
        self.peers.get(&peer).and_then(|p| p.declared_rate_bytes)
    }
}

/// One replicated entity as the pipeline sees it this tick.
#[derive(Debug, Clone)]
pub struct InterestEntity {
    /// Its id.
    pub id: NetId,
    /// Its dense slot (stable while the entity lives, see `crate::dense`).
    pub slot: u32,
    /// The server entity.
    pub entity: Entity,
    /// The resolved owner.
    pub owner: PeerId,
    /// Depth below replicated roots (parents are evaluated first).
    pub depth: u32,
    /// Something replicated changed this tick (its position is read again).
    pub dirty: bool,
    /// Its `Replicated` component or parent changed (its profile and link are resolved again).
    pub structure: bool,
    /// It has `NetPredicted`.
    pub predicted: bool,
    /// The replicated parent.
    pub parent: Option<NetId>,
}

/// The cold per-entity inputs.
#[derive(Debug, Clone)]
struct EntityInfo {
    id: NetId,
    entity: Entity,
    depth: u32,
    profile_name: Option<String>,
    cell: Option<GridAt>,
    parent: Option<NetId>,
    link: Option<NetId>,
    /// The update (by its counter) that last listed the entity.
    seen: u64,
}

/// The per-entity inputs every connection reads, one array element per slot.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SlotHot {
    pos: Vec3,
    id: NetId,
    owner: PeerId,
    profile: u32,
    has_pos: bool,
    live: bool,
    predicted: bool,
    /// Takes the short evaluation: no profile flag, filter or list applies to it.
    plain: bool,
}

impl SlotHot {
    /// An update without `every` has nothing but a new position for this entity: no structure
    /// change, no profile that reads other entities, and a place in the grid already.
    fn only_moves(&self, ie: &InterestEntity) -> bool {
        self.live && self.plain && self.has_pos && !ie.structure
    }
}

impl Default for SlotHot {
    fn default() -> Self {
        Self {
            pos: Vec3::ZERO,
            id: NetId(0),
            owner: PeerId::SERVER,
            profile: 0,
            has_pos: false,
            live: false,
            predicted: false,
            plain: false,
        }
    }
}

/// One resolved profile in the table entities index.
#[derive(Debug, Clone)]
struct ProfileRow {
    key: String,
    profile: NetProfile,
    /// `relevance: Spatial` radius and `radius × (1 + hysteresis)`; `None` for world relevance.
    spatial: Option<(f32, f32)>,
    /// No flag or filter beyond the spatial or world test.
    plain: bool,
    /// The grid band of `relevance: Spatial` (`u32::MAX` otherwise).
    band: u32,
}

/// Per (connection, entity) state, public view. See [`PeerInterest::entity_state`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PeerEntityState {
    /// The spatial test passed last tick (hysteresis).
    pub spatial_in: bool,
    /// Ticks of linger left (`Some` while lingering).
    pub linger_left: Option<u32>,
    /// Distance to the nearest viewpoint.
    pub distance: Option<f32>,
    /// Inside a viewpoint's forward cone.
    pub in_view: bool,
    /// The stage that decided the last evaluation.
    pub stage: Option<RelevanceStage>,
}

const FLAG_HELD: u8 = 1;
const FLAG_SPATIAL_IN: u8 = 1 << 1;
const FLAG_IN_VIEW: u8 = 1 << 2;
const FLAG_RELEVANT: u8 = 1 << 3;
const FLAG_HAS_DISTANCE: u8 = 1 << 4;
const NOT_LINGERING: u32 = u32::MAX;
const NOT_SENT: u32 = u32::MAX;
const NO_STAGE: u8 = u8::MAX;

/// Per (connection, held entity) state as stored, 20 bytes (spec 72 § 14 allows 24): relevance,
/// linger and the priority accumulator.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PeerSlot {
    pub(crate) acc: f32,
    pub(crate) distance: f32,
    last_sent: u32,
    linger_left: u32,
    flags: u8,
    stage: u8,
}

impl Default for PeerSlot {
    fn default() -> Self {
        Self {
            acc: 0.0,
            distance: 0.0,
            last_sent: NOT_SENT,
            linger_left: NOT_LINGERING,
            flags: 0,
            stage: NO_STAGE,
        }
    }
}

impl PeerSlot {
    fn state(&self) -> PeerEntityState {
        PeerEntityState {
            spatial_in: self.flags & FLAG_SPATIAL_IN != 0,
            linger_left: (self.linger_left != NOT_LINGERING).then_some(self.linger_left),
            distance: (self.flags & FLAG_HAS_DISTANCE != 0).then_some(self.distance),
            in_view: self.flags & FLAG_IN_VIEW != 0,
            stage: RelevanceStage::ALL.get(usize::from(self.stage)).copied(),
        }
    }

    pub(crate) fn distance(&self) -> Option<f32> {
        (self.flags & FLAG_HAS_DISTANCE != 0).then_some(self.distance)
    }

    pub(crate) fn in_view(&self) -> bool {
        self.flags & FLAG_IN_VIEW != 0
    }

    pub(crate) fn last_sent(&self) -> Option<NetTick> {
        (self.last_sent != NOT_SENT).then_some(NetTick(self.last_sent))
    }

    pub(crate) fn set_last_sent(&mut self, tick: NetTick) {
        self.last_sent = tick.0;
    }
}

/// Fingerprint of one connection's held ids, linger class and viewpoints. Same inputs, same
/// stamp, and the mix allocates nothing. The prime is the public FNV-1a prime.
fn peer_snapshot_stamp(pi: &PeerInterest) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for (id, slot) in &pi.held {
        let lingering = pi
            .slots
            .get(*slot)
            .is_some_and(|s| s.linger_left != NOT_LINGERING);
        hash = hash.wrapping_mul(0x100_0000_01b3).wrapping_add(u64::from(id.0));
        hash = hash
            .wrapping_mul(0x100_0000_01b3)
            .wrapping_add(u64::from(u8::from(lingering)));
    }
    for (at, forward) in &pi.view.viewpoints {
        for component in [at.x, at.y, at.z, forward.x, forward.y, forward.z] {
            hash = hash
                .wrapping_mul(0x100_0000_01b3)
                .wrapping_add(u64::from(component.to_bits()));
        }
    }
    hash
}

/// One connection's relevance state.
#[derive(Debug, Clone, Default)]
pub struct PeerInterest {
    /// Per held entity.
    pub(crate) slots: HeldSlots<PeerSlot>,
    /// Relevant and lingering entities, by `NetId`: `(id, slot)`.
    pub(crate) held: Vec<(NetId, u32)>,
    /// The bits of `held`.
    pub(crate) held_bits: SlotBits,
    /// Candidates of the running update.
    scratch: SlotBits,
    /// The next `held`, kept between updates for its buffer.
    next_held: Vec<(NetId, u32)>,
    /// Slots that became relevant on the last evaluation.
    pub(crate) gained_slots: Vec<u32>,
    /// Slots that left on the last evaluation.
    pub(crate) lost_slots: Vec<u32>,
    /// This tick's view.
    pub view: PeerView,
    /// Send-side state (budget, bandwidth, counters).
    pub send: ConnSendState,
    /// Fingerprint of this connection's held set and viewpoints.
    gizmo_stamp: Option<u64>,
    /// The viewpoints of the last evaluation: unchanged viewpoints keep the verdicts of held
    /// entities that did not move either.
    last_viewpoints: Vec<(Vec3, Vec3)>,
    /// The newcomers of the running evaluation, kept between updates for its buffer.
    fresh: Vec<(NetId, u32)>,
    /// The gather's priority ranking of this connection, kept between ticks for its buffer.
    pub(crate) pack_ranked: Vec<crate::replication::Ranked>,
    /// The slots the gather picked for this connection, kept between ticks for its buffer.
    pub(crate) pack_picked: Vec<u32>,
}

impl PeerInterest {
    /// Whether the entity is relevant or lingering for this connection.
    pub fn holds(&self, id: NetId) -> bool {
        self.held_slot(id).is_some()
    }

    fn held_slot(&self, id: NetId) -> Option<u32> {
        self.held
            .binary_search_by_key(&id, |(i, _)| *i)
            .ok()
            .and_then(|at| self.held.get(at))
            .map(|(_, s)| *s)
    }

    /// Whether the entity is lingering.
    pub fn is_lingering(&self, id: NetId) -> bool {
        self.entity_state(id).is_some_and(|s| s.linger_left.is_some())
    }

    /// Entities relevant or lingering for this connection.
    pub fn held_count(&self) -> usize {
        self.held.len()
    }

    /// The ids relevant or lingering for this connection, in order.
    pub fn held_ids(&self) -> impl Iterator<Item = NetId> + '_ {
        self.held.iter().map(|(id, _)| *id)
    }

    /// The state of a relevant or lingering entity.
    pub fn entity_state(&self, id: NetId) -> Option<PeerEntityState> {
        let slot = self.held_slot(id)?;
        self.slots.get(slot).map(PeerSlot::state)
    }

    /// The send state of a relevant or lingering entity.
    pub fn send_state(&self, id: NetId) -> Option<EntitySendState> {
        let slot = self.held_slot(id)?;
        self.slots.get(slot).map(|s| EntitySendState {
            acc: s.acc,
            last_sent: s.last_sent(),
        })
    }

    /// `(id, state)` of every relevant or lingering entity, in id order.
    pub fn entity_states(&self) -> impl Iterator<Item = (NetId, PeerEntityState)> + '_ {
        self.held
            .iter()
            .filter_map(|(id, slot)| self.slots.get(*slot).map(|s| (*id, s.state())))
    }

    /// `(id, send state)` of every relevant or lingering entity, in id order.
    pub fn send_states(&self) -> impl Iterator<Item = (NetId, EntitySendState)> + '_ {
        self.held.iter().filter_map(|(id, slot)| {
            self.slots.get(*slot).map(|s| {
                (
                    *id,
                    EntitySendState {
                        acc: s.acc,
                        last_sent: s.last_sent(),
                    },
                )
            })
        })
    }
}

/// Counters of the relevance layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InterestCounters {
    /// Pipeline evaluations this tick (candidates over every connection).
    pub evaluations: u64,
    /// Slots the grid's band queries marked this tick, over every connection (candidates before
    /// the distance test; spec 120 § 8.4: each band is queried with its own radius).
    pub grid_marked: u64,
    /// Held entities this tick kept their verdict without an evaluation: neither they nor the
    /// connection's viewpoints moved (spec 120 § 8.4).
    pub held_kept: u64,
    /// Profiles naming an unknown custom filter (logged once each).
    pub filter_unknown: u64,
}

/// The relevance state of a server (spec 72). Insert it to turn the pipeline on.
#[derive(Resource, Debug, Default)]
pub struct NetInterest {
    /// Cold inputs per slot (`None` = free).
    info: Vec<Option<EntityInfo>>,
    /// Hot inputs per slot.
    hot: Vec<SlotHot>,
    /// Slots by id.
    ids: HashMap<NetId, u32>,
    /// Resolved profiles (rebuilt when `NetProfiles` changes).
    profile_table: Vec<ProfileRow>,
    /// Table rows by the resolved key.
    profile_rows: HashMap<String, u32>,
    /// The shared grid.
    pub grid: InterestGrid,
    /// `relevance: World` or `sent_to_all` entities.
    world_list: BTreeSet<u32>,
    /// `follow_parent` entities.
    link_list: BTreeSet<u32>,
    /// The bits of `link_list`, this update.
    link_bits: SlotBits,
    /// Slots carrying `NetVisibility` or `NetGroups`, or named by a grant, this tick.
    listed: SlotBits,
    /// Slots whose entity carries a `NetProfileRef`, sorted.
    profile_refs: Vec<u32>,
    /// Temporary grants.
    pub grants: Vec<RelevanceGrant>,
    /// Per connection.
    pub peers: BTreeMap<PeerId, PeerInterest>,
    /// Slots whose inputs changed this update (moved, structure, new or gone).
    touched: SlotBits,
    /// This update saw every entity: every slot counts as touched.
    all_touched: bool,
    warned_filters: BTreeSet<String>,
    /// Group-name copies `peer_views` made. An unchanged second update copies none.
    group_name_copies: u64,
    /// Unknown custom-filter names copied during evaluation.
    filter_name_copies: u64,
    profiles_seen: Option<u64>,
    updates: u64,
    /// Counters.
    pub counters: InterestCounters,
    /// The tick of the last update.
    pub tick: NetTick,
    /// Ticks since the previous update (linger runs in ticks, updates on send ticks).
    elapsed: u32,
    /// Wall time of the last update's per-connection part.
    pub(crate) fan_out_ms: f64,
    /// Slots some connection started holding at the last update.
    pub(crate) gained: Vec<u32>,
    /// Slots owned by each connection (every entity walks through here only on a full update).
    owned_by: HashMap<PeerId, Vec<u32>>,
    /// Bumps when the grid cell, the profile generation, or any connection's held set or
    /// viewpoints change. The relevance gizmo skips its mark-list rebuild while this is unchanged.
    gizmo_gen: u64,
    /// Profile generation last folded into `gizmo_gen`.
    gizmo_profiles: Option<u64>,
    /// Grid cell size, as `f32` bits, last folded into `gizmo_gen`.
    gizmo_cell: Option<u32>,
}

/// Turns the spec 72 relevance, priority and budget pipeline on for a server world. The project
/// default profile comes from `config/net.ron` (`replication.default_profile`) when `NetSettings`
/// is present now; `NetProfiles::set_default` changes it later.
pub fn install_net_interest(world: &mut World) {
    world.init_resource::<NetInterest>();
    world.init_resource::<NetRelevanceFilters>();
    if world.get_resource::<NetProfiles>().is_none() {
        let mut profiles = NetProfiles::default();
        if let Some(s) = world.get_resource::<crate::settings::NetSettings>() {
            profiles.set_default(&s.replication.default_profile);
        }
        world.insert_resource(profiles);
    }
}

/// `net_grant_relevance(entity, target, until_tick)`: makes an entity relevant to a peer or group
/// until the tick (server). Returns `false` without a `NetInterest` resource.
pub fn net_grant_relevance(world: &mut World, entity: Entity, target: GrantTarget, until: NetTick) -> bool {
    match world.get_resource_mut::<NetInterest>() {
        Some(mut i) => {
            i.grants.push(RelevanceGrant {
                entity,
                target,
                until,
            });
            true
        }
        None => false,
    }
}

fn world_pos(world: &World, e: Entity) -> Option<Vec3> {
    let ent = world.get_entity(e).ok()?;
    if ent.get::<ChildOf>().is_some()
        && let Some(g) = ent.get::<GlobalTransform>()
    {
        return Some(g.0.translation.into());
    }
    ent.get::<Transform>().map(|t| t.translation)
}

/// The unit forward direction of a viewpoint, from the same frame as [`world_pos`]: the global
/// rotation of a child (a camera on a yawed pawn looks where the pawn looks), the local one of a
/// root. A degenerate rotation falls back to world -Z.
fn world_forward(world: &World, e: Entity) -> Vec3 {
    let Ok(ent) = world.get_entity(e) else {
        return Vec3::NEG_Z;
    };
    let raw = if ent.get::<ChildOf>().is_some()
        && let Some(g) = ent.get::<GlobalTransform>()
    {
        g.0.transform_vector3(Vec3::NEG_Z)
    } else {
        ent.get::<Transform>()
            .map_or(Vec3::NEG_Z, |t| t.rotation * Vec3::NEG_Z)
    };
    raw.try_normalize().unwrap_or(Vec3::NEG_Z)
}

/// `true` when `set` holds exactly `names`, with no extra element.
fn same_names(set: &HashSet<String>, names: &[String]) -> bool {
    set.len() == names.len() && names.iter().all(|n| set.contains(n))
}

fn peer_views(
    world: &mut World,
    peers: &[PeerId],
    owned_predicted: &HashMap<PeerId, Vec<Vec3>>,
    prev: &mut HashMap<PeerId, PeerView>,
    copies: &mut u64,
) -> HashMap<PeerId, PeerView> {
    let session = world.get_resource::<NetSessionPeers>();
    let mut out: HashMap<PeerId, PeerView> = peers
        .iter()
        .map(|p| {
            // The last view's viewpoint list keeps its buffer (spec 120 § 8.4: no per-connection
            // allocation per update).
            let mut viewpoints = prev
                .get_mut(p)
                .map(|old| std::mem::take(&mut old.viewpoints))
                .unwrap_or_default();
            viewpoints.clear();
            let view = PeerView {
                not_in_game: session.is_some_and(|s| !s.in_game(*p)),
                viewpoints,
                ..PeerView::default()
            };
            (*p, view)
        })
        .collect();
    let mut q = world.query::<(Entity, &NetViewpoint)>();
    let vps: Vec<(Entity, PeerId)> = q.iter(world).map(|(e, v)| (e, v.player.peer)).collect();
    for (e, peer) in vps {
        let Some(view) = out.get_mut(&peer) else {
            continue;
        };
        let Some(pos) = world_pos(world, e) else {
            continue;
        };
        view.viewpoints.push((pos, world_forward(world, e)));
    }
    for (peer, view) in out.iter_mut() {
        if view.viewpoints.is_empty()
            && let Some(ps) = owned_predicted.get(peer)
        {
            view.viewpoints.extend(ps.iter().map(|p| (*p, Vec3::NEG_Z)));
        }
    }
    let mut q = world.query::<(&NetPlayer, Option<&NetGroupVisibility>, Option<&NetGroups>)>();
    for (p, vis, groups) in q.iter(world) {
        let Some(view) = out.get_mut(&p.id.peer) else {
            continue;
        };
        if let Some(v) = vis {
            let reuse_allow = view.group_allow.is_empty()
                && prev
                    .get(&p.id.peer)
                    .is_some_and(|old| same_names(&old.group_allow, &v.allow));
            let reuse_deny = view.group_deny.is_empty()
                && prev
                    .get(&p.id.peer)
                    .is_some_and(|old| same_names(&old.group_deny, &v.deny));
            if reuse_allow && let Some(old) = prev.get_mut(&p.id.peer) {
                view.group_allow = std::mem::take(&mut old.group_allow);
            } else {
                *copies += v.allow.len() as u64;
                view.group_allow.extend(v.allow.iter().cloned());
            }
            if reuse_deny && let Some(old) = prev.get_mut(&p.id.peer) {
                view.group_deny = std::mem::take(&mut old.group_deny);
            } else {
                *copies += v.deny.len() as u64;
                view.group_deny.extend(v.deny.iter().cloned());
            }
        }
        if let Some(g) = groups {
            let reuse = view.groups.is_empty()
                && prev
                    .get(&p.id.peer)
                    .is_some_and(|old| same_names(&old.groups, &g.0));
            if reuse && let Some(old) = prev.get_mut(&p.id.peer) {
                view.groups = std::mem::take(&mut old.groups);
            } else {
                *copies += g.0.len() as u64;
                view.groups.extend(g.0.iter().cloned());
            }
        }
    }
    out
}

/// Most worker chunks one relevance update or gather splits connections into.
pub const MAX_RELEVANCE_THREADS: usize = 8;

/// Passes over link followers until their relevance is stable (chains through `NetOwnerLink`).
pub const MAX_LINK_PASSES: usize = 8;

/// The cosine of half the 120° forward cone.
const VIEW_CONE_COS: f32 = 0.5;

struct EvalInput<'a> {
    world: &'a World,
    peer: PeerId,
    view: &'a PeerView,
    filters: &'a NetRelevanceFilters,
    grants: &'a [RelevanceGrant],
}

/// Distance to the nearest viewpoint and whether the point is in a forward cone.
///
/// The cone test uses squared lengths. One square root is taken, of the nearest viewpoint, because
/// the stored distance is in metres.
#[inline]
fn view_geometry(pos: Vec3, viewpoints: &[(Vec3, Vec3)]) -> (f32, bool) {
    let mut best_sq = f32::INFINITY;
    let mut seen = false;
    let cos2 = VIEW_CONE_COS * VIEW_CONE_COS;
    for (vp, fwd) in viewpoints {
        let d = pos - *vp;
        let len_sq = d.length_squared();
        if len_sq < best_sq {
            best_sq = len_sq;
        }
        // `len <= 1e-6` is `len_sq <= 1e-12`. A point on the viewpoint is inside the cone.
        if len_sq <= 1e-12 {
            seen = true;
            continue;
        }
        let dot = d.dot(*fwd);
        if dot > 0.0 && dot * dot >= cos2 * len_sq {
            seen = true;
        }
    }
    let best = if best_sq.is_finite() {
        best_sq.sqrt()
    } else {
        f32::INFINITY
    };
    (best, seen)
}

/// Squared-distance factor above which a candidate is out of range whatever the rounding of the
/// exact test (`distance <= radius` on the square root).
const CLEAR_MISS: f32 = 1.000_01;

/// A slot leaving one grid cell for another (`None` = no cell).
type CellMove = (u32, Option<GridAt>, Option<GridAt>);

/// Entities an update must see before its work is worth a task.
const PARALLEL_POSITIONS: usize = 512;

/// The world positions of the entities that moved, by the index in `entities` (`None` for the rest).
fn moved_positions(world: &World, entities: &[InterestEntity]) -> Vec<Option<Vec3>> {
    let mut out = vec![None; entities.len()];
    let read = |from: usize, part: &mut [Option<Vec3>]| {
        for (p, ie) in part.iter_mut().zip(entities.get(from..).unwrap_or(&[])) {
            if ie.dirty {
                *p = world_pos(world, ie.entity);
            }
        }
    };
    if entities.len() < PARALLEL_POSITIONS {
        read(0, &mut out);
    } else {
        let chunk = crate::dense::worker_chunk(entities.len());
        regia_core::tasks::par_chunks_mut(&mut out, chunk, |i, part| read(i * chunk, part));
    }
    out
}

/// Merges two sorted lists into `out`.
fn merge_sorted(a: &[(NetId, u32)], b: &[(NetId, u32)], out: &mut Vec<(NetId, u32)>) {
    out.clear();
    out.reserve(a.len() + b.len());
    let (mut a, mut b) = (a.iter().peekable(), b.iter().peekable());
    loop {
        let next = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) if y < x => b.next(),
            (Some(_), _) => a.next(),
            _ => b.next(),
        };
        match next {
            Some(item) => out.push(*item),
            None => return,
        }
    }
}

/// What one connection's update reports back.
struct PeerEval {
    unknown: Vec<String>,
    evaluations: u64,
    marked: u64,
    kept: u64,
}

impl NetInterest {
    fn slot_row(&self, slot: u32) -> Option<&ProfileRow> {
        let hot = self.hot.get(slot as usize)?;
        self.profile_table.get(hot.profile as usize)
    }

    /// Group names copied while building peer views, since the pipeline was created.
    pub fn group_name_copies(&self) -> u64 {
        self.group_name_copies
    }

    /// Unknown custom-filter names copied while evaluating, since the pipeline was created.
    pub fn filter_name_copies(&self) -> u64 {
        self.filter_name_copies
    }

    /// The tracked profile key and profile of an entity.
    pub fn profile_of(&self, id: NetId) -> Option<(&str, &NetProfile)> {
        let slot = *self.ids.get(&id)?;
        self.slot_row(slot).map(|r| (r.key.as_str(), &r.profile))
    }

    /// The profile table row of a slot (the send loop keeps it with the tick's entity array).
    pub(crate) fn profile_row(&self, slot: u32) -> u32 {
        self.hot.get(slot as usize).map_or(u32::MAX, |h| h.profile)
    }

    /// A profile by table row.
    pub(crate) fn profile_in_row(&self, row: u32) -> Option<&NetProfile> {
        self.profile_table.get(row as usize).map(|r| &r.profile)
    }

    /// Whether a connection holds (relevant or lingering) an entity.
    pub fn holds(&self, peer: PeerId, id: NetId) -> bool {
        self.peers.get(&peer).is_some_and(|p| p.holds(id))
    }

    /// The users count of each profile key.
    pub fn profile_users(&self) -> BTreeMap<String, usize> {
        let mut out = BTreeMap::new();
        for h in self.hot.iter().filter(|h| h.live) {
            if let Some(r) = self.profile_table.get(h.profile as usize) {
                *out.entry(r.key.clone()).or_insert(0) += 1;
            }
        }
        out
    }

    /// Evaluates one (connection, entity) by § 5.1, without linger. `link_relevant` is the
    /// relevance of the entity's link target this tick (link rule).
    fn evaluate(
        &self,
        cx: &EvalInput<'_>,
        slot: u32,
        prev_spatial: bool,
        link_relevant: Option<bool>,
        want_detail: bool,
        filter_unknown: &mut Vec<String>,
    ) -> Option<RelevanceVerdict> {
        // Details are text for `why`; the per-tick pass skips building them.
        let txt = |s: &str| if want_detail { s.to_string() } else { String::new() };
        let hot = self.hot.get(slot as usize).filter(|h| h.live)?;
        let entity = self.info.get(slot as usize)?.as_ref()?.entity;
        let row = self.profile_table.get(hot.profile as usize)?;
        let p = &row.profile;
        let (distance, in_view) = if hot.has_pos && !cx.view.viewpoints.is_empty() {
            let (d, seen) = view_geometry(hot.pos, &cx.view.viewpoints);
            (Some(d), seen)
        } else {
            (None, false)
        };
        let verdict =
            |relevant: bool, stage: RelevanceStage, detail: String, spatial_in: bool| RelevanceVerdict {
                relevant,
                stage,
                detail,
                distance,
                in_view,
                spatial_in,
            };
        // 1. Owner rule.
        if hot.owner == cx.peer {
            return Some(verdict(
                true,
                RelevanceStage::Owner,
                txt("owned by the peer"),
                prev_spatial,
            ));
        }
        let vis = cx.world.get::<NetVisibility>(entity);
        let groups = cx.world.get::<NetGroups>(entity);
        // 2. Denies.
        if vis.is_some_and(|v| v.deny.contains(&cx.peer)) {
            return Some(verdict(
                false,
                RelevanceStage::Visibility,
                txt("peer is in the deny list"),
                false,
            ));
        }
        if let Some(g) = groups.and_then(|g| g.0.iter().find(|n| cx.view.group_deny.contains(*n))) {
            return Some(verdict(
                false,
                RelevanceStage::Group,
                if want_detail {
                    format!("group `{g}` is denied")
                } else {
                    String::new()
                },
                false,
            ));
        }
        // 3. Owner-exclusive.
        if p.owner_only {
            return Some(verdict(
                false,
                RelevanceStage::OwnerOnly,
                txt("owner_only profile"),
                false,
            ));
        }
        // 4. Grants. `sent_to_all` reaches connections in `InGame` only (spec 72 § 5.1).
        if p.sent_to_all {
            return Some(if cx.view.not_in_game {
                verdict(
                    false,
                    RelevanceStage::SentToAll,
                    txt("sent_to_all profile: the peer is not in game yet"),
                    false,
                )
            } else {
                verdict(true, RelevanceStage::SentToAll, txt("sent_to_all profile"), false)
            });
        }
        if let Some(g) = cx.grants.iter().find(|g| {
            g.entity == entity
                && match &g.target {
                    GrantTarget::Peer(p) => *p == cx.peer,
                    GrantTarget::Group(n) => cx.view.groups.contains(n),
                }
        }) {
            return Some(verdict(
                true,
                RelevanceStage::Grant,
                if want_detail {
                    format!("granted until tick {}", g.until.0)
                } else {
                    String::new()
                },
                prev_spatial,
            ));
        }
        if p.follow_parent {
            let r = link_relevant.unwrap_or(false);
            let detail = if r {
                "link target is relevant"
            } else {
                "link target is not relevant"
            };
            return Some(verdict(r, RelevanceStage::Link, txt(detail), false));
        }
        // 5. Filters: allow list, group_required, spatial, custom.
        if let Some(v) = vis
            && !v.allow.is_empty()
            && !v.allow.contains(&cx.peer)
        {
            return Some(verdict(
                false,
                RelevanceStage::Visibility,
                txt("peer is not in the allow list"),
                false,
            ));
        }
        if p.group_required && !groups.is_some_and(|g| g.0.iter().any(|n| cx.view.group_allow.contains(n))) {
            return Some(verdict(
                false,
                RelevanceStage::Group,
                txt("no allowed group (group_required)"),
                false,
            ));
        }
        let spatial_in = match p.relevance {
            RelevanceShape::World => true,
            RelevanceShape::Spatial { radius, hysteresis } => match distance {
                None => {
                    let why = if cx.view.viewpoints.is_empty() {
                        "the peer has no viewpoint"
                    } else {
                        "no position"
                    };
                    return Some(verdict(false, RelevanceStage::Spatial, txt(why), false));
                }
                Some(d) => {
                    let limit = spatial_limit(radius, hysteresis, prev_spatial);
                    if d > limit {
                        return Some(verdict(
                            false,
                            RelevanceStage::Spatial,
                            if want_detail {
                                format!("{d:.2} m from the nearest viewpoint, limit {limit:.2} m")
                            } else {
                                String::new()
                            },
                            false,
                        ));
                    }
                    true
                }
            },
        };
        if let Some(name) = &p.custom_filter {
            match cx.filters.get(name) {
                Some(f) => {
                    if !f(cx.world, entity, cx.peer) {
                        return Some(verdict(
                            false,
                            RelevanceStage::Custom,
                            if want_detail {
                                format!("filter `{name}` refused")
                            } else {
                                String::new()
                            },
                            spatial_in,
                        ));
                    }
                }
                // A name already logged is not copied again. Evaluation runs in parallel, so this
                // only reads the set; the caller inserts into it after the workers finish.
                None if !self.warned_filters.contains(name) => filter_unknown.push(name.clone()),
                None => {}
            }
        }
        let detail = match (p.relevance, distance) {
            _ if !want_detail => String::new(),
            (RelevanceShape::World, _) => "world relevance".to_string(),
            (_, Some(d)) => format!("{d:.2} m from the nearest viewpoint"),
            _ => String::new(),
        };
        Some(verdict(true, RelevanceStage::Spatial, detail, spatial_in))
    }

    /// Rebuilds the profile table's row for a resolved profile.
    fn row_of(&mut self, key: String, profile: NetProfile) -> u32 {
        if let Some(i) = self.profile_rows.get(&key) {
            return *i;
        }
        let spatial = match profile.relevance {
            RelevanceShape::Spatial { radius, hysteresis } => Some((radius, radius * (1.0 + hysteresis))),
            RelevanceShape::World => None,
        };
        let plain = !profile.owner_only
            && !profile.sent_to_all
            && !profile.follow_parent
            && !profile.group_required
            && profile.custom_filter.is_none();
        let band = spatial.map_or(u32::MAX, |(radius, _)| self.grid.band_of(radius));
        let i = self.profile_table.len() as u32;
        self.profile_rows.insert(key.clone(), i);
        self.profile_table.push(ProfileRow {
            key,
            profile,
            spatial,
            plain,
            band,
        });
        i
    }

    /// Refreshes entity inputs and the grid, then evaluates every connection's candidates and
    /// applies linger. `entities` are every replicated entity this tick, in `NetId` order.
    pub fn update(
        &mut self,
        world: &mut World,
        entities: &[InterestEntity],
        peers: &[PeerId],
        tick: NetTick,
    ) {
        self.update_from(world, entities, true, &[], peers, tick);
    }

    /// The entities of an update without `every` that only moved (`SlotHot::only_moves`): nothing
    /// but their position and grid cell change, so this runs on the pool over the array of
    /// positions. `entities` is in slot order. Everything else stays for the serial pass.
    fn follow_movers(&mut self, world: &World, entities: &[InterestEntity]) {
        let Self {
            hot,
            info,
            grid,
            profile_table,
            ..
        } = self;
        let cells: &InterestGrid = &*grid;
        let tables: &[ProfileRow] = profile_table.as_slice();
        let walk = |first: usize, part: &mut [SlotHot]| {
            let mut crossed: Vec<CellMove> = Vec::new();
            let from = entities.partition_point(|ie| (ie.slot as usize) < first);
            for ie in entities.get(from..).unwrap_or(&[]) {
                let Some(h) = (ie.slot as usize)
                    .checked_sub(first)
                    .and_then(|at| part.get_mut(at))
                else {
                    break;
                };
                if !h.only_moves(ie) {
                    continue;
                }
                h.predicted = ie.predicted;
                if !ie.dirty {
                    continue;
                }
                let pos = world_pos(world, ie.entity);
                let band = tables
                    .get(h.profile as usize)
                    .and_then(|r| r.spatial.map(|_| r.band));
                let old_cell = band.map(|b| (b, cells.cell_of(h.pos)));
                let new_cell = band.and_then(|b| pos.map(|p| (b, cells.cell_of(p))));
                if old_cell != new_cell {
                    crossed.push((ie.slot, old_cell, new_cell));
                }
                h.has_pos = pos.is_some();
                h.pos = pos.unwrap_or(h.pos);
            }
            crossed
        };
        let crossed: Vec<_> = if entities.len() < PARALLEL_POSITIONS {
            vec![walk(0, hot.as_mut_slice())]
        } else {
            let chunk = crate::dense::worker_chunk(hot.len());
            regia_core::tasks::par_chunks_mut(hot.as_mut_slice(), chunk, |i, part| walk(i * chunk, part))
        };
        for (slot, old_cell, new_cell) in crossed.into_iter().flatten() {
            if let Some(c) = old_cell {
                grid.remove_at(c, slot);
            }
            if let Some(c) = new_cell {
                grid.insert_at(c, slot);
            }
            if let Some(Some(i)) = info.get_mut(slot as usize) {
                i.cell = new_cell;
            }
        }
    }

    /// Who carries a `NetProfileRef` now, and which of them changed name.
    fn profile_carriers(&self, world: &mut World) -> (Vec<u32>, BTreeSet<u32>) {
        let mut carriers: Vec<u32> = Vec::new();
        let mut stale_refs: BTreeSet<u32> = BTreeSet::new();
        // The query walks only those entities.
        let mut q = world.query::<(&NetId, &NetProfileRef)>();
        for (id, r) in q.iter(world) {
            let Some(slot) = self.ids.get(id) else {
                continue;
            };
            carriers.push(*slot);
            let named = self
                .info
                .get(*slot as usize)
                .and_then(Option::as_ref)
                .and_then(|i| i.profile_name.as_deref());
            if named != Some(r.0.as_str()) {
                stale_refs.insert(*slot);
            }
        }
        carriers.sort_unstable();
        (carriers, stale_refs)
    }

    /// Whether the next update must see every entity: profiles, the grid or an override changed,
    /// so entities that did not change need work too.
    pub(crate) fn needs_every_entity(&self, world: &mut World) -> bool {
        let cell_m = world
            .get_resource::<crate::settings::NetSettings>()
            .map_or(64.0, |s| s.replication.grid_cell_m);
        let generation = world.get_resource::<NetProfiles>().map(NetProfiles::generation);
        if self.updates == 0
            || (self.grid.cell_m - cell_m).abs() > f32::EPSILON
            || generation.is_none()
            || generation != self.profiles_seen
        {
            return true;
        }
        let (carriers, stale_refs) = self.profile_carriers(world);
        !stale_refs.is_empty() || carriers != self.profile_refs
    }

    /// Forgets a slot whose entity is gone: its grid cell, lists and per-slot inputs.
    fn forget_slot(&mut self, slot: u32) {
        let index = slot as usize;
        let Some(Some(info)) = self.info.get(index) else {
            return;
        };
        let (id, cell) = (info.id, info.cell);
        if let Some(c) = cell {
            self.grid.remove_at(c, slot);
        }
        if let Some(h) = self.hot.get(index)
            && h.owner != PeerId::SERVER
            && let Some(list) = self.owned_by.get_mut(&h.owner)
        {
            list.retain(|s| *s != slot);
        }
        if let Some(i) = self.info.get_mut(index) {
            *i = None;
        }
        if let Some(h) = self.hot.get_mut(index) {
            *h = SlotHot::default();
        }
        self.ids.remove(&id);
        self.world_list.remove(&slot);
        self.link_list.remove(&slot);
        self.profile_refs.retain(|s| *s != slot);
        self.touched.set(slot);
    }

    /// One update. `entities` lists every entity when `every` is set; otherwise only the entities
    /// that moved, changed structure or changed owner, on a tick where
    /// [`NetInterest::needs_every_entity`] is false, with `gone` naming the slots despawned since
    /// the last update (spawns and despawns no longer force a full pass, spec 120 § 8.4). An
    /// unchanged entity's inputs are still right then, and a tick's serial part follows what
    /// changed.
    pub(crate) fn update_from(
        &mut self,
        world: &mut World,
        entities: &[InterestEntity],
        every: bool,
        gone: &[u32],
        peers: &[PeerId],
        tick: NetTick,
    ) {
        let _span = tracing::trace_span!("net_relevance").entered();
        self.elapsed = if self.updates == 0 {
            1
        } else {
            tick.0.saturating_sub(self.tick.0).max(1)
        };
        self.tick = tick;
        self.updates += 1;
        let stamp = self.updates;
        self.touched.reset(self.hot.len());
        self.all_touched = every;
        for ie in entities {
            if ie.dirty || ie.structure {
                self.touched.set(ie.slot);
            }
        }
        if !every {
            // A full update finds what is gone by its own scan below.
            for slot in gone {
                self.forget_slot(*slot);
            }
        }
        let cell_m = world
            .get_resource::<crate::settings::NetSettings>()
            .map_or(64.0, |s| s.replication.grid_cell_m);
        if (self.grid.cell_m - cell_m).abs() > f32::EPSILON {
            // A new cell size rebuilds the grid (the bands keep their radii, rows keep their bands).
            self.grid.reset(cell_m);
            for i in self.info.iter_mut().flatten() {
                i.cell = None;
            }
        }
        world.init_resource::<NetProfiles>();
        world.init_resource::<NetRelevanceFilters>();
        let mut profiles = world.remove_resource::<NetProfiles>().unwrap_or_default();
        let profiles_changed = Some(profiles.generation()) != self.profiles_seen;
        if profiles_changed {
            // A profile was loaded, replaced or failed, or the default moved: every entity
            // resolves its profile again.
            self.profiles_seen = Some(profiles.generation());
            self.profile_table.clear();
            self.profile_rows.clear();
        }
        // Who carries a runtime profile override now.
        let (carriers, mut stale_refs) = self.profile_carriers(world);
        if carriers != self.profile_refs {
            // A carrier lost its override, or a new one appeared.
            stale_refs.extend(
                self.profile_refs
                    .iter()
                    .filter(|s| carriers.binary_search(s).is_err()),
            );
            self.profile_refs = carriers;
        }
        // Inputs. Reading what moved from the world is the largest serial part of an update, so
        // those reads go first, on the pool.
        if every {
            self.owned_by.clear();
        }
        let moved = if every {
            moved_positions(world, entities)
        } else {
            self.follow_movers(world, entities);
            Vec::new()
        };
        for (at, ie) in entities.iter().enumerate() {
            let index = ie.slot as usize;
            if !every && self.hot.get(index).is_some_and(|h| h.only_moves(ie)) {
                continue;
            }
            if index >= self.info.len() {
                self.info.resize(index + 1, None);
                self.hot.resize(index + 1, SlotHot::default());
            }
            let fresh = self.info.get(index).is_none_or(Option::is_none);
            if fresh && let Some(old) = self.info.get_mut(index) {
                *old = Some(EntityInfo {
                    id: ie.id,
                    entity: ie.entity,
                    depth: ie.depth,
                    profile_name: None,
                    cell: None,
                    parent: ie.parent,
                    link: None,
                    seen: stamp,
                });
                self.ids.insert(ie.id, ie.slot);
            }
            let refresh_profile = fresh || profiles_changed || ie.structure || stale_refs.contains(&ie.slot);
            if refresh_profile {
                // `NetProfileRef` (runtime override) or `Replicated.profile` (saved with the entity).
                let name = world
                    .get::<NetProfileRef>(ie.entity)
                    .map(|r| r.0.clone())
                    .or_else(|| {
                        world
                            .get::<crate::replication::Replicated>(ie.entity)
                            .and_then(|r| r.profile.clone())
                    });
                let (key, p) = profiles.resolve(name.as_deref());
                let row = self.row_of(key, p);
                if let (Some(Some(info)), Some(hot)) = (self.info.get_mut(index), self.hot.get_mut(index)) {
                    info.profile_name = name;
                    hot.profile = row;
                }
            }
            let (Some(Some(info)), Some(hot)) = (self.info.get_mut(index), self.hot.get_mut(index)) else {
                continue;
            };
            info.seen = stamp;
            info.depth = ie.depth;
            hot.id = ie.id;
            if !every && (fresh || hot.owner != ie.owner) {
                // A full update rebuilds `owned_by`; a partial one moves the slot between lists.
                if !fresh
                    && hot.owner != PeerId::SERVER
                    && let Some(list) = self.owned_by.get_mut(&hot.owner)
                {
                    list.retain(|s| *s != ie.slot);
                }
                if ie.owner != PeerId::SERVER {
                    self.owned_by.entry(ie.owner).or_default().push(ie.slot);
                }
                self.touched.set(ie.slot);
            }
            hot.owner = ie.owner;
            hot.live = true;
            hot.predicted = ie.predicted;
            if fresh || ie.dirty || !hot.has_pos {
                let pos = match moved.get(at) {
                    Some(p) if ie.dirty => *p,
                    _ => world_pos(world, ie.entity),
                };
                match pos {
                    Some(p) => {
                        hot.pos = p;
                        hot.has_pos = true;
                    }
                    None => hot.has_pos = false,
                }
            }
            let Some(row) = self.profile_table.get(hot.profile as usize) else {
                continue;
            };
            hot.plain = row.plain;
            let follow = row.profile.follow_parent;
            let sent_to_all = row.profile.sent_to_all;
            let is_world = row.profile.relevance == RelevanceShape::World;
            let spatial = !is_world && !sent_to_all && !follow;
            let band = row.band;
            let cell_now = (spatial && hot.has_pos).then(|| (band, self.grid.cell_of(hot.pos)));
            info.parent = ie.parent;
            if follow {
                info.link = info.parent.or_else(|| {
                    world
                        .get::<NetOwnerLink>(ie.entity)
                        .and_then(|l| world.get::<NetId>(l.0).copied())
                });
            }
            if cell_now != info.cell {
                if let Some(c) = info.cell {
                    self.grid.remove_at(c, ie.slot);
                }
                if let Some(c) = cell_now {
                    self.grid.insert_at(c, ie.slot);
                }
                info.cell = cell_now;
            }
            if refresh_profile {
                if !follow && (sent_to_all || is_world) {
                    self.world_list.insert(ie.slot);
                } else {
                    self.world_list.remove(&ie.slot);
                }
                if follow {
                    self.link_list.insert(ie.slot);
                } else {
                    self.link_list.remove(&ie.slot);
                }
            }
            if every && ie.owner != PeerId::SERVER {
                self.owned_by.entry(ie.owner).or_default().push(ie.slot);
            }
        }
        // Predicted pawns move, so their positions are read every update.
        let mut owned_predicted: HashMap<PeerId, Vec<Vec3>> = HashMap::new();
        for (peer, slots) in &self.owned_by {
            let pawns = slots
                .iter()
                .filter_map(|s| self.hot.get(*s as usize))
                .filter(|h| h.predicted && h.has_pos)
                .map(|h| h.pos);
            let pawns: Vec<Vec3> = pawns.collect();
            if !pawns.is_empty() {
                owned_predicted.insert(*peer, pawns);
            }
        }
        // Entities gone since the last tick.
        let scanned = if every { self.info.len() } else { 0 };
        for index in 0..scanned {
            let Some(Some(info)) = self.info.get(index) else {
                continue;
            };
            if info.seen == stamp {
                continue;
            }
            self.forget_slot(index as u32);
        }
        world.insert_resource(profiles);
        // Grants expire.
        self.grants.retain(|g| g.until > tick);
        // Slots that take the long evaluation this tick: entities with visibility lists, groups
        // or grants. The queries walk only those entities.
        self.listed.reset(self.hot.len());
        {
            let mark = |id: Option<&NetId>, listed: &mut SlotBits, all: &mut bool| match id {
                Some(id) => {
                    if let Some(s) = self.ids.get(id) {
                        listed.set(*s);
                    }
                }
                None => *all = true,
            };
            let mut listed = std::mem::take(&mut self.listed);
            let mut unmapped = false;
            let mut q = world.query_filtered::<Option<&NetId>, With<NetVisibility>>();
            for id in q.iter(world) {
                mark(id, &mut listed, &mut unmapped);
            }
            let mut q = world.query_filtered::<Option<&NetId>, With<NetGroups>>();
            for id in q.iter(world) {
                mark(id, &mut listed, &mut unmapped);
            }
            for g in &self.grants {
                match world.get::<NetId>(g.entity) {
                    Some(id) => mark(Some(id), &mut listed, &mut unmapped),
                    None => unmapped = true,
                }
            }
            if unmapped {
                // A driver without ids (a test on the pipeline alone): every slot goes the long way.
                for s in 0..self.hot.len() as u32 {
                    listed.set(s);
                }
            }
            self.listed = listed;
        }
        // Connections.
        self.peers.retain(|p, _| peers.contains(p));
        for p in peers {
            self.peers.entry(*p).or_default();
        }
        let mut prev_views = HashMap::new();
        for (peer, pi) in &mut self.peers {
            prev_views.insert(*peer, std::mem::take(&mut pi.view));
        }
        let mut group_copies = 0u64;
        let mut views = peer_views(world, peers, &owned_predicted, &mut prev_views, &mut group_copies);
        self.group_name_copies += group_copies;
        // Link followers in depth order (parents decide first).
        let mut links: Vec<(u32, u32)> = self
            .link_list
            .iter()
            .filter_map(|slot| {
                let i = self.info.get(*slot as usize)?.as_ref()?;
                Some((i.depth, *slot))
            })
            .collect();
        links.sort_unstable();
        self.link_bits.reset(self.hot.len());
        for (_, s) in &links {
            self.link_bits.set(*s);
        }
        let filters = world.resource::<NetRelevanceFilters>().clone();
        let world_ref: &World = world;
        let mut work: Vec<(PeerId, PeerInterest)> = std::mem::take(&mut self.peers).into_iter().collect();
        let session = world.get_resource::<NetSessionPeers>();
        for (peer, pi) in &mut work {
            pi.view = views.remove(peer).unwrap_or_default();
            pi.send.session_rate = session.and_then(|s| s.declared_rate(*peer));
        }
        let fan_out = std::time::Instant::now();
        let this: &NetInterest = self;
        let chunk = crate::dense::worker_chunk(work.len());
        // One connection's evaluation reads the shared state only, so connections run in parallel
        // and each result equals a serial run (spec 72 § 11).
        let eval = |peer: PeerId, pi: &mut PeerInterest| {
            this.eval_peer(peer, pi, world_ref, &filters, &links, &this.owned_by)
        };
        let results: Vec<Vec<PeerEval>> = crate::dense::par_mut(&mut work, chunk, |part| {
            part.iter_mut().map(|(p, pi)| eval(*p, pi)).collect()
        });
        self.peers = work.into_iter().collect();
        self.fan_out_ms = fan_out.elapsed().as_secs_f64() * 1000.0;
        let mut unknown: Vec<String> = Vec::new();
        let (mut evaluations, mut marked, mut kept) = (0u64, 0u64, 0u64);
        self.gained.clear();
        for r in results.into_iter().flatten() {
            evaluations += r.evaluations;
            marked += r.marked;
            kept += r.kept;
            unknown.extend(r.unknown);
        }
        for pi in self.peers.values() {
            self.gained.extend_from_slice(&pi.gained_slots);
        }
        self.counters.evaluations = evaluations;
        self.counters.grid_marked = marked;
        self.counters.held_kept = kept;
        self.filter_name_copies += unknown.len() as u64;
        for name in unknown {
            if self.warned_filters.insert(name.clone()) {
                self.counters.filter_unknown += 1;
                tracing::warn!(target: "net", filter = %name, "net.filter_unknown");
            }
        }
        let profile_gen = world.get_resource::<NetProfiles>().map(NetProfiles::generation);
        self.note_gizmo_generation(profile_gen);
    }

    /// Bumps [`Self::generation`] when the gizmo's inputs changed.
    fn note_gizmo_generation(&mut self, profile_gen: Option<u64>) {
        let mut dirty = false;
        if self.gizmo_profiles != profile_gen {
            self.gizmo_profiles = profile_gen;
            dirty = true;
        }
        let cell = self.grid.cell_m.to_bits();
        if self.gizmo_cell != Some(cell) {
            self.gizmo_cell = Some(cell);
            dirty = true;
        }
        for pi in self.peers.values_mut() {
            let stamp = peer_snapshot_stamp(pi);
            if pi.gizmo_stamp != Some(stamp) {
                pi.gizmo_stamp = Some(stamp);
                dirty = true;
            }
        }
        if dirty {
            self.gizmo_gen = self.gizmo_gen.wrapping_add(1);
        }
    }

    /// Generation of the inputs the `net_relevance` gizmo reads. Unchanged while the grid cell,
    /// the profile generation, and every connection's held set and viewpoints stay the same.
    pub fn generation(&self) -> u64 {
        self.gizmo_gen
    }

    /// Evaluates one connection's candidates into its per-slot state and held list.
    fn eval_peer(
        &self,
        peer: PeerId,
        pi: &mut PeerInterest,
        world: &World,
        filters: &NetRelevanceFilters,
        links: &[(u32, u32)],
        owned_by: &HashMap<PeerId, Vec<u32>>,
    ) -> PeerEval {
        let count = self.hot.len();
        // Candidates: a bit per slot, so duplicates cost nothing and the order is fixed.
        let PeerInterest {
            slots,
            held,
            held_bits,
            scratch: cand,
            next_held,
            view,
            gained_slots,
            lost_slots,
            last_viewpoints,
            fresh: fresh_buf,
            ..
        } = pi;
        cand.reset(count);
        if let Some(owned) = owned_by.get(&peer) {
            for s in owned {
                cand.set(*s);
            }
        }
        for s in &self.world_list {
            cand.set(*s);
        }
        // Each band with its own radius (spec 120 § 8.4): a long-range profile's few entities do
        // not widen the query of every other profile.
        let mut marked = 0u64;
        for (vp, _) in &view.viewpoints {
            marked += self.grid.query_bands(*vp, |s| cand.set(s));
        }
        // Unchanged viewpoints and an entity that did not move: the last verdict stands.
        let view_same = !self.all_touched && *last_viewpoints == view.viewpoints;
        for g in &self.grants {
            if let Some(s) = world.get::<NetId>(g.entity).and_then(|id| self.ids.get(id)) {
                cand.set(*s);
            }
        }
        for (_, s) in links {
            cand.unset(*s);
        }
        // What is held goes through its packed state below; the rest of the bits are newcomers.
        cand.and_not(held_bits);
        let view: &PeerView = view;
        let cx = EvalInput {
            world,
            peer,
            view,
            filters,
            grants: &self.grants,
        };
        let mut unknown = Vec::new();
        let mut evaluations = 0u64;
        let mut kept = 0u64;
        let elapsed = self.elapsed;
        let viewpoints = view.viewpoints.as_slice();
        let settle = |st: &mut PeerSlot, slot: u32, v: Verdict, fresh: &mut Vec<(NetId, u32)>| {
            let held_before = st.flags & FLAG_HELD != 0;
            let linger_left = if v.relevant {
                NOT_LINGERING
            } else if !held_before {
                // Not held before: nothing to linger.
                st.flags = 0;
                return;
            } else {
                let ticks = self.slot_row(slot).map_or(0, |r| r.profile.linger_ticks);
                let left = if st.linger_left == NOT_LINGERING {
                    ticks
                } else {
                    st.linger_left
                };
                if left == 0 {
                    *st = PeerSlot::default();
                    return;
                }
                left.saturating_sub(elapsed)
            };
            // Zero left lingers this last tick; it is dropped on the next evaluation.
            st.linger_left = linger_left;
            st.stage = v.stage;
            st.distance = v.distance.unwrap_or(0.0);
            let mut flags = FLAG_HELD;
            if v.relevant {
                flags |= FLAG_RELEVANT;
            }
            if v.spatial_in {
                flags |= FLAG_SPATIAL_IN;
            }
            if v.in_view {
                flags |= FLAG_IN_VIEW;
            }
            if v.distance.is_some() {
                flags |= FLAG_HAS_DISTANCE;
            }
            st.flags = flags;
            if !held_before && let Some(h) = self.hot.get(slot as usize) {
                fresh.push((h.id, slot));
            }
        };
        // The common case takes the distance test alone (the long path gives the same answer).
        let is_plain = |slot: u32, hot: &SlotHot| {
            hot.plain && hot.has_pos && !viewpoints.is_empty() && hot.owner != peer && !self.listed.get(slot)
        };
        let mut decide =
            |slot: u32, hot: &SlotHot, plain: bool, st: &mut PeerSlot, fresh: &mut Vec<(NetId, u32)>| {
                let prev = st.flags & FLAG_SPATIAL_IN != 0;
                if plain {
                    let row = self.profile_table.get(hot.profile as usize);
                    let (d, seen) = view_geometry(hot.pos, viewpoints);
                    let (relevant, spatial_in) = match row.and_then(|r| r.spatial) {
                        None => (true, true),
                        Some((radius, limit)) => {
                            let inside = d <= if prev { limit } else { radius };
                            (inside, inside)
                        }
                    };
                    let v = Verdict {
                        relevant,
                        spatial_in,
                        in_view: seen,
                        distance: Some(d),
                        stage: RelevanceStage::Spatial.index(),
                    };
                    settle(st, slot, v, fresh);
                    return;
                }
                match self.evaluate(&cx, slot, prev, None, false, &mut unknown) {
                    Some(v) => settle(st, slot, Verdict::of(&v), fresh),
                    None => *st = PeerSlot::default(),
                }
            };
        // Held entities, in the order of their packed state: most of a connection's work, and the
        // visit order does not matter because `held` keeps its own order.
        let mut lost: Vec<u32> = std::mem::take(lost_slots);
        lost.clear();
        let mut fresh: Vec<(NetId, u32)> = std::mem::take(fresh_buf);
        fresh.clear();
        let has_links = !links.is_empty();
        let spatial_stage = RelevanceStage::Spatial.index();
        let (owners, values) = slots.parts_mut();
        for (slot, st) in owners.iter().copied().zip(values) {
            if has_links && self.link_bits.get(slot) {
                continue;
            }
            let Some(hot) = self.hot.get(slot as usize) else {
                continue;
            };
            if !hot.live {
                // Despawned: its state goes with it.
                lost.push(slot);
                continue;
            }
            if view_same
                && st.linger_left == NOT_LINGERING
                && st.stage == spatial_stage
                && !self.touched.get(slot)
                && is_plain(slot, hot)
            {
                kept += 1;
                continue;
            }
            evaluations += 1;
            decide(slot, hot, is_plain(slot, hot), st, &mut fresh);
            if st.flags & FLAG_HELD == 0 {
                lost.push(slot);
            }
        }
        // Newcomers. Most candidates of a grid query are out of range, so they have no state to
        // read or change. Only a clear miss takes the short exit: a distance near the radius goes
        // through the exact test.
        cand.for_each(|slot| {
            let Some(hot) = self.hot.get(slot as usize).filter(|h| h.live) else {
                return;
            };
            evaluations += 1;
            let plain = is_plain(slot, hot);
            let spatial = self
                .profile_table
                .get(hot.profile as usize)
                .and_then(|r| r.spatial);
            if plain
                && spatial.is_some_and(|(radius, _)| {
                    let clear = radius * radius * CLEAR_MISS;
                    viewpoints
                        .iter()
                        .all(|(vp, _)| hot.pos.distance_squared(*vp) > clear)
                })
            {
                return;
            }
            let mut st = PeerSlot::default();
            decide(slot, hot, plain, &mut st, &mut fresh);
            if st.flags & FLAG_HELD != 0 {
                slots.insert(slot, st);
            }
        });
        // Link followers: in depth order, again until stable (a `NetOwnerLink` target may be a
        // follower evaluated later); at most `MAX_LINK_PASSES` passes.
        if !links.is_empty() {
            // Nothing carries over from the last tick: a cycle of followers stays irrelevant.
            for (_, slot) in links {
                if let Some(st) = slots.entry(*slot) {
                    st.flags &= !FLAG_RELEVANT;
                }
            }
            let mut linked: Vec<(u32, RelevanceVerdict)> = Vec::with_capacity(links.len());
            for _ in 0..MAX_LINK_PASSES {
                let mut changed = false;
                linked.clear();
                for (_, slot) in links {
                    let target = self
                        .info
                        .get(*slot as usize)
                        .and_then(Option::as_ref)
                        .and_then(|i| i.link)
                        .and_then(|t| self.ids.get(&t));
                    let link_relevant = self
                        .info
                        .get(*slot as usize)
                        .and_then(Option::as_ref)
                        .and_then(|i| i.link)
                        .map(|_| {
                            target
                                .and_then(|t| slots.get(*t))
                                .is_some_and(|s| s.flags & FLAG_RELEVANT != 0)
                        });
                    if let Some(v) = self.evaluate(&cx, *slot, false, link_relevant, false, &mut unknown) {
                        evaluations += 1;
                        if let Some(st) = slots.get_mut(*slot) {
                            let was = st.flags & FLAG_RELEVANT != 0;
                            changed |= was != v.relevant;
                            if v.relevant {
                                st.flags |= FLAG_RELEVANT;
                            } else {
                                st.flags &= !FLAG_RELEVANT;
                            }
                        }
                        linked.push((*slot, v));
                    }
                }
                if !changed {
                    break;
                }
            }
            for (slot, v) in &linked {
                if let Some(st) = slots.get_mut(*slot) {
                    settle(st, *slot, Verdict::of(v), &mut fresh);
                }
            }
            for (_, slot) in links {
                if slots.get(*slot).is_some_and(|st| st.flags & FLAG_HELD == 0) {
                    lost.push(*slot);
                }
            }
        }
        // `held` stays sorted by id: a tick that changed nothing costs nothing here.
        let mut shrank = false;
        for slot in &lost {
            slots.remove(*slot);
            shrank |= held_bits.get(*slot);
            held_bits.unset(*slot);
        }
        if shrank {
            held.retain(|(_, s)| held_bits.get(*s));
        }
        gained_slots.clear();
        if !fresh.is_empty() {
            fresh.sort_unstable();
            merge_sorted(held, &fresh, next_held);
            std::mem::swap(held, next_held);
            for (_, s) in &fresh {
                held_bits.set(*s);
            }
            gained_slots.extend(fresh.iter().map(|(_, s)| *s));
        }
        *lost_slots = lost;
        *fresh_buf = fresh;
        if !view_same {
            last_viewpoints.clear();
            last_viewpoints.extend_from_slice(&view.viewpoints);
        }
        PeerEval {
            unknown,
            evaluations,
            marked,
            kept,
        }
    }

    /// `net.why_not_relevant`: evaluates one (connection, entity) now, by the full pipeline.
    pub fn why(&self, world: &World, peer: PeerId, id: NetId) -> Option<RelevanceVerdict> {
        let pi = self.peers.get(&peer)?;
        let filters = world
            .get_resource::<NetRelevanceFilters>()
            .cloned()
            .unwrap_or_default();
        let cx = EvalInput {
            world,
            peer,
            view: &pi.view,
            filters: &filters,
            grants: &self.grants,
        };
        let slot = *self.ids.get(&id)?;
        let prev = pi.entity_state(id).is_some_and(|s| s.spatial_in);
        let link = self
            .info
            .get(slot as usize)
            .and_then(Option::as_ref)
            .and_then(|i| i.link)
            .map(|t| pi.entity_state(t).is_some_and(|s| s.linger_left.is_none()));
        let mut unknown = Vec::new();
        self.evaluate(&cx, slot, prev, link, true, &mut unknown)
    }

    /// Brute force: the relevant set of a connection by evaluating every tracked entity (no grid;
    /// the parity test and the agent).
    pub fn brute_force(&self, world: &World, peer: PeerId) -> BTreeSet<NetId> {
        let Some(pi) = self.peers.get(&peer) else {
            return BTreeSet::new();
        };
        let filters = world
            .get_resource::<NetRelevanceFilters>()
            .cloned()
            .unwrap_or_default();
        let cx = EvalInput {
            world,
            peer,
            view: &pi.view,
            filters: &filters,
            grants: &self.grants,
        };
        let mut unknown = Vec::new();
        let mut out = BTreeSet::new();
        for (id, slot) in &self.ids {
            if self.link_list.contains(slot) {
                continue;
            }
            let prev = pi.entity_state(*id).is_some_and(|s| s.spatial_in);
            if self
                .evaluate(&cx, *slot, prev, None, false, &mut unknown)
                .is_some_and(|v| v.relevant)
            {
                out.insert(*id);
            }
        }
        let mut links: Vec<(u32, u32)> = self
            .link_list
            .iter()
            .filter_map(|s| Some((self.info.get(*s as usize)?.as_ref()?.depth, *s)))
            .collect();
        links.sort_unstable();
        let mut link_rel: HashMap<NetId, bool> = HashMap::new();
        for _ in 0..MAX_LINK_PASSES {
            let mut changed = false;
            for (_, slot) in &links {
                let Some(Some(info)) = self.info.get(*slot as usize) else {
                    continue;
                };
                let lr = info
                    .link
                    .map(|t| out.contains(&t) || link_rel.get(&t).copied().unwrap_or(false));
                let r = self
                    .evaluate(&cx, *slot, false, lr, false, &mut unknown)
                    .is_some_and(|v| v.relevant);
                changed |= link_rel.insert(info.id, r) != Some(r);
            }
            if !changed {
                break;
            }
        }
        out.extend(link_rel.into_iter().filter(|(_, r)| *r).map(|(id, _)| id));
        out
    }
}

/// The outcome of one evaluation, as `settle` stores it.
#[derive(Clone, Copy)]
struct Verdict {
    relevant: bool,
    spatial_in: bool,
    in_view: bool,
    distance: Option<f32>,
    stage: u8,
}

impl Verdict {
    fn of(v: &RelevanceVerdict) -> Self {
        Self {
            relevant: v.relevant,
            spatial_in: v.spatial_in,
            in_view: v.in_view,
            distance: v.distance,
            stage: v.stage.index(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A camera child of a pawn yawed 90 degrees looks along the pawn's global -Z (world -X), not
    /// the world -Z its local rotation gives; the vector is unit length.
    #[test]
    fn a_child_viewpoint_looks_along_its_global_forward() {
        let mut world = World::new();
        let yaw = glam::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        let parent = world.spawn_empty().id();
        let child = world
            .spawn((
                ChildOf(parent),
                Transform::IDENTITY,
                GlobalTransform(glam::Affine3A::from_rotation_translation(
                    yaw,
                    Vec3::new(1.0, 2.0, 3.0),
                )),
            ))
            .id();
        let fwd = world_forward(&world, child);
        assert!((fwd - Vec3::NEG_X).length() < 1e-5, "{fwd}");
        assert!((fwd.length() - 1.0).abs() < 1e-5);
        assert_eq!(world_pos(&world, child), Some(Vec3::new(1.0, 2.0, 3.0)));
        // A root uses its own rotation; a scaled global still yields a unit vector.
        let root = world.spawn(Transform::from_rotation(yaw)).id();
        assert!((world_forward(&world, root) - Vec3::NEG_X).length() < 1e-5);
        let scaled = world
            .spawn((
                ChildOf(parent),
                GlobalTransform(glam::Affine3A::from_scale(Vec3::splat(4.0))),
            ))
            .id();
        assert!((world_forward(&world, scaled) - Vec3::NEG_Z).length() < 1e-5);
    }

    #[test]
    fn merging_keeps_the_order() {
        let list = |ids: &[u32]| ids.iter().map(|i| (NetId(*i), *i)).collect::<Vec<_>>();
        let mut out = vec![(NetId(99), 99)];
        merge_sorted(&list(&[1, 4, 9]), &list(&[2, 3, 10]), &mut out);
        assert_eq!(out, list(&[1, 2, 3, 4, 9, 10]));
        merge_sorted(&[], &list(&[5]), &mut out);
        assert_eq!(out, list(&[5]));
        merge_sorted(&list(&[5]), &[], &mut out);
        assert_eq!(out, list(&[5]));
    }
}
