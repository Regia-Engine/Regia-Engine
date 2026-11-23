// SPDX-License-Identifier: MPL-2.0
//! Fog of war over the network: a server sends each client only what its team sees (audit gap
//! FW-5, book/agents/fog-of-war.md § "Fog over the network").
//!
//! [`fog_view`](crate::fog_view) hides enemies from one local view. In a server-authoritative or
//! listen-server game that is not enough: the replication layer would still send every hidden
//! enemy to every client, and a modified client could draw them. This module connects the fog rules
//! to the net relevance pipeline (`regia_net::relevance`, spec 72 § 5.1 stage 6): a custom relevance
//! filter named [`FOG_RELEVANCE_FILTER`] that removes, per connection, the enemies the connection's
//! team cannot see. A removed entity is never replicated to that client: no spawn, no component, no
//! position.
//!
//! How it fits together:
//!
//! - Server entities that fog may hide carry a [`FogUnit`] (team, kind, [`SightClass`], alive) and
//!   a `Transform`; their replication profile names the filter (`custom_filter: Some("fog")`, or the
//!   built-in [`FOG_RELEVANCE_PROFILE`] that [`install_fog_relevance`] registers). Projectiles and
//!   effects carry a [`FogEffect`] naming their source.
//! - The game maps each connection to a fog team in [`FogTeamOf`] (`PeerId -> TeamId`). Team 0 (or a
//!   team above [`FogRules::team_count`]) observes everything. A connection with no team is refused
//!   every fogged entity (fail closed), its own entities excepted (the net owner rule runs first).
//! - [`fog_relevance_prepare`] runs once per server tick, before the send: it builds one
//!   [`TeamVision`] per mapped team (shared by every connection of
//!   that team), applies the grace rules below, and stores each team's shown set. The filter itself
//!   is a set lookup, so the per-connection cost does not grow with the watcher count.
//!
//! Grace (in server ticks, [`FogRelevanceSettings`]):
//!
//! - **Linger**: an enemy that leaves sight stays relevant for `linger_ticks` more ticks, so the
//!   client's interpolation does not pop it out mid-motion. Give the replication profile
//!   `linger_ticks: 0` (the built-in profile does): the profile's own linger runs after the filter
//!   refuses, and would add to this one.
//! - **Lead**: an enemy becomes relevant up to `lead_ticks` before it enters sight. The test is a
//!   second vision whose sight and detection radii are widened by
//!   `lead_ticks x max_step_m` metres ([`widened_sight`]): an enemy that moves at most `max_step_m`
//!   per tick and is outside that ring cannot enter sight within `lead_ticks` ticks. The lead
//!   vision asks the same line-of-sight hook, so it never reveals through walls; an enemy that
//!   steps around a corner can still appear on the tick it is seen.
//!
//! Events: an entity carrying [`FogEffect`] (a projectile, an explosion) is shown to a team when
//! its source is shown to that team or its own position is in that team's sight, under
//! [`FogEventPolicy::HideUnseen`] (the default); `PassAll` shows every effect. Messages a server
//! sends itself ask [`fog_event_shown`] with the event's [`EventPlace`] first.
//!
//! Lockstep peers hold the full state by design: fog there is presentation only, and this module
//! does not apply. A competitive lockstep game needs server validation, which is out of scope here.
//! A listen-server host holds the full state too (it is the server); hide enemies from its local
//! view with [`FogViewSim`](crate::fog_view::FogViewSim) or the game's own presentation filter.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_detsim::fog::{FogCounters, TeamVision};
use regia_net::{
    NetProfile, NetProfiles, NetRelevanceFilter, NetSet, PeerId, RelevanceShape, add_net_relevance_filter,
    on_server,
};
use regia_scene::transform::{GlobalTransform, Transform};

pub use crate::fog_view::FogEventPolicy;
use crate::fog_view::{
    FogEntity, FogRules, FogWorld, LineOfSight, OpenGround, SightClass, TeamId, widened_sight,
};
use crate::sim_bridge::EventPlace;

/// The name the fog relevance filter is registered under (`custom_filter: Some("fog")` in a
/// replication profile).
pub const FOG_RELEVANCE_FILTER: &str = "fog";

/// The name of the built-in replication profile [`install_fog_relevance`] adds when the world has
/// `NetProfiles`: [`fog_relevance_profile`].
pub const FOG_RELEVANCE_PROFILE: &str = "fog";

/// A replication profile for fogged entities: world relevance (fog decides, not distance), the
/// [`FOG_RELEVANCE_FILTER`], and no profile linger (the fog's own `linger_ticks` applies). Every
/// other field is the profile default.
pub fn fog_relevance_profile() -> NetProfile {
    NetProfile {
        relevance: RelevanceShape::World,
        custom_filter: Some(FOG_RELEVANCE_FILTER.to_string()),
        linger_ticks: 0,
        ..NetProfile::default()
    }
}

/// On a server entity fog may hide: what fog needs to know about it. Its position is its world
/// translation (`GlobalTransform` for a child, else `Transform`); an entity with neither is not
/// shown to any enemy team (fail closed).
#[derive(Component, Debug, Clone, PartialEq)]
pub struct FogUnit {
    /// The owning fog team (0 is neutral: always shown).
    pub team: TeamId,
    /// The kind key, matched against the sight table's kind rows.
    pub kind: String,
    /// The sight class.
    pub class: SightClass,
    /// False for a destroyed entity (a wreck): it watches nothing and is always shown, so the
    /// events of its death reach every team.
    pub alive: bool,
}

impl FogUnit {
    /// A living entity of `team`.
    pub fn new(team: TeamId, kind: impl Into<String>, class: SightClass) -> Self {
        Self {
            team,
            kind: kind.into(),
            class,
            alive: true,
        }
    }
}

/// On a projectile or effect entity whose replication profile uses the fog filter: it follows
/// [`FogEventPolicy`]. Under `HideUnseen` it is shown to a team while its `source` is shown to that
/// team or its own position is inside that team's sight; under `PassAll` it is always shown.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FogEffect {
    /// The entity that fired or caused it, if any.
    pub source: Option<Entity>,
}

/// The fog team of each connection, filled by the game (from its lobby or team assignment).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct FogTeamOf {
    teams: BTreeMap<PeerId, TeamId>,
}

impl FogTeamOf {
    /// Puts `peer` on `team` (0 observes everything).
    pub fn set(&mut self, peer: PeerId, team: TeamId) {
        self.teams.insert(peer, team);
    }

    /// Takes `peer` off its team; it is then refused every fogged entity.
    pub fn remove(&mut self, peer: PeerId) -> Option<TeamId> {
        self.teams.remove(&peer)
    }

    /// The team of `peer`.
    pub fn get(&self, peer: PeerId) -> Option<TeamId> {
        self.teams.get(&peer).copied()
    }

    /// `(peer, team)` in peer order.
    pub fn iter(&self) -> impl Iterator<Item = (PeerId, TeamId)> + '_ {
        self.teams.iter().map(|(p, t)| (*p, *t))
    }

    /// Mapped connections.
    pub fn len(&self) -> usize {
        self.teams.len()
    }

    /// True when no connection is mapped.
    pub fn is_empty(&self) -> bool {
        self.teams.is_empty()
    }
}

/// The grace rules and alliances of the fog relevance (see the module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct FogRelevanceSettings {
    /// Ticks an enemy stays relevant after it leaves sight (interpolation does not pop).
    pub linger_ticks: u32,
    /// Ticks before entering sight an enemy becomes relevant (0 turns the lead off).
    pub lead_ticks: u32,
    /// The farthest any fogged entity moves in one server tick, in metres: the lead margin is
    /// `lead_ticks x max_step_m`.
    pub max_step_m: f32,
    /// What happens to effects ([`FogEffect`]) and [`fog_event_shown`] questions.
    pub events: FogEventPolicy,
    /// Allied team pairs (sight is shared both ways).
    pub alliances: Vec<(TeamId, TeamId)>,
}

impl Default for FogRelevanceSettings {
    fn default() -> Self {
        Self {
            linger_ticks: 6,
            lead_ticks: 4,
            max_step_m: 0.5,
            events: FogEventPolicy::HideUnseen,
            alliances: Vec::new(),
        }
    }
}

impl FogRelevanceSettings {
    /// The lead margin in metres: `lead_ticks x max_step_m`, or 0 when that is not a positive
    /// finite number.
    pub fn lead_margin_m(&self) -> f32 {
        let m = self.lead_ticks as f32 * self.max_step_m;
        if m.is_finite() && m > 0.0 { m } else { 0.0 }
    }

    fn allied(&self, a: TeamId, b: TeamId) -> bool {
        self.alliances
            .iter()
            .any(|&(x, y)| (x == a && y == b) || (x == b && y == a))
    }
}

/// The last tick's work and results for one team.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TeamFogStats {
    /// The team.
    pub team: TeamId,
    /// Connections mapped to it.
    pub peers: u64,
    /// Fogged entities shown to it (seen, lingering or in the lead ring, plus the always-shown).
    pub shown: u64,
    /// Fogged entities hidden from it.
    pub hidden: u64,
    /// Entities shown only because they linger after leaving sight.
    pub linger_hits: u64,
    /// Entities shown only because they are inside the lead ring.
    pub lead_hits: u64,
    /// Living own or allied watchers of its vision.
    pub watchers: u64,
    /// Effects ([`FogEffect`]) shown to it.
    pub effects_shown: u64,
    /// Effects hidden from it.
    pub effects_hidden: u64,
    /// Sight queries, index candidates and line-of-sight calls of its visions this tick.
    pub queries: u64,
}

/// Running totals for one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerFogStats {
    /// The connection.
    pub peer: PeerId,
    /// Its team.
    pub team: TeamId,
    /// Fogged entities the filter was asked about for it, so far.
    pub evaluated: u64,
    /// Of those, the ones it refused (hidden from the connection), so far.
    pub filtered: u64,
}

/// A snapshot of the fog relevance for the agent (`fog.status` block `net`). Counts only: never a
/// hidden entity's id, kind or position.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FogRelevanceStatus {
    /// Fog on.
    pub enabled: bool,
    /// Player teams are `1..=team_count`.
    pub team_count: TeamId,
    /// [`FogRelevanceSettings::linger_ticks`].
    pub linger_ticks: u32,
    /// [`FogRelevanceSettings::lead_ticks`].
    pub lead_ticks: u32,
    /// [`FogRelevanceSettings::lead_margin_m`].
    pub lead_margin_m: f32,
    /// `hide_unseen` or `pass_all`.
    pub event_policy: &'static str,
    /// The line-of-sight hook's name.
    pub line_of_sight: String,
    /// Server ticks prepared so far.
    pub ticks: u64,
    /// Fogged entities ([`FogUnit`]) in the last tick.
    pub units: u64,
    /// Fogged entities without a position in the last tick (hidden from every enemy).
    pub unplaced: u64,
    /// Effects ([`FogEffect`]) in the last tick.
    pub effects: u64,
    /// Sight visions built in the last tick: one per mapped player team.
    pub visions_built: u64,
    /// Lead visions built in the last tick: one per mapped player team while the lead is on.
    pub lead_visions_built: u64,
    /// Sight visions built so far.
    pub visions_total: u64,
    /// Per team, the last tick.
    pub teams: Vec<TeamFogStats>,
    /// Per mapped connection, running totals.
    pub peers: Vec<PeerFogStats>,
    /// Filter refusals of connections with no team, so far.
    pub unmapped_refusals: u64,
    /// Filter refusals of entities with neither [`FogUnit`] nor [`FogEffect`], so far.
    pub unlisted_refusals: u64,
    /// Linger hits over every team, so far.
    pub linger_hits_total: u64,
    /// Lead hits over every team, so far.
    pub lead_hits_total: u64,
    /// [`fog_event_shown`] answers `true`, so far.
    pub events_passed: u64,
    /// [`fog_event_shown`] answers `false`, so far.
    pub events_dropped: u64,
}

/// One team's prepared fog.
#[derive(Default)]
struct TeamFog {
    vision: TeamVision,
    lead: TeamVision,
    /// Shown this tick (seen, lingering, lead, always-shown, shown effects).
    shown: HashSet<Entity>,
    /// Linger ticks left, and the tick the entity was last listed.
    linger: HashMap<Entity, (u32, u64)>,
    stats: TeamFogStats,
}

/// Per connection counters the filter writes (it only has `&World`).
#[derive(Default)]
struct PeerCounters {
    team: TeamId,
    evaluated: AtomicU64,
    filtered: AtomicU64,
}

/// The server's fog relevance: rules, grace settings, line of sight and the per-team state the
/// filter reads. Insert with [`install_fog_relevance`] (or [`FogRelevancePlugin`]).
#[derive(Resource)]
pub struct FogRelevance {
    rules: FogRules,
    lead_rules: FogRules,
    settings: FogRelevanceSettings,
    los: Arc<dyn LineOfSight + Send + Sync>,
    los_name: String,
    teams: BTreeMap<TeamId, TeamFog>,
    peers: BTreeMap<PeerId, PeerCounters>,
    ticks: u64,
    units: u64,
    unplaced: u64,
    effects: u64,
    visions_built: u64,
    lead_visions_built: u64,
    visions_total: u64,
    linger_hits_total: u64,
    lead_hits_total: u64,
    unmapped: AtomicU64,
    unlisted: AtomicU64,
    events_passed: AtomicU64,
    events_dropped: AtomicU64,
}

impl FogRelevance {
    /// Fog relevance under `rules` and `settings`, over open ground.
    pub fn new(rules: FogRules, settings: FogRelevanceSettings) -> Self {
        let lead_rules = widened_sight(&rules, settings.lead_margin_m());
        Self {
            rules,
            lead_rules,
            settings,
            los: Arc::new(OpenGround),
            los_name: "open_ground".to_string(),
            teams: BTreeMap::new(),
            peers: BTreeMap::new(),
            ticks: 0,
            units: 0,
            unplaced: 0,
            effects: 0,
            visions_built: 0,
            lead_visions_built: 0,
            visions_total: 0,
            linger_hits_total: 0,
            lead_hits_total: 0,
            unmapped: AtomicU64::new(0),
            unlisted: AtomicU64::new(0),
            events_passed: AtomicU64::new(0),
            events_dropped: AtomicU64::new(0),
        }
    }

    /// Uses `los` for line of sight (sight and lead); `name` is what `fog.status` reports.
    pub fn with_line_of_sight(
        mut self,
        name: impl Into<String>,
        los: Arc<dyn LineOfSight + Send + Sync>,
    ) -> Self {
        self.set_line_of_sight(name, los);
        self
    }

    /// Replaces the line-of-sight hook from the next tick.
    pub fn set_line_of_sight(&mut self, name: impl Into<String>, los: Arc<dyn LineOfSight + Send + Sync>) {
        self.los = los;
        self.los_name = name.into();
    }

    /// The fog rules.
    pub fn rules(&self) -> &FogRules {
        &self.rules
    }

    /// Replaces the fog rules from the next tick.
    pub fn set_rules(&mut self, rules: FogRules) {
        self.lead_rules = widened_sight(&rules, self.settings.lead_margin_m());
        self.rules = rules;
    }

    /// The grace settings.
    pub fn settings(&self) -> &FogRelevanceSettings {
        &self.settings
    }

    /// Replaces the grace settings from the next tick. Lingering entities keep the ticks they had
    /// left.
    pub fn set_settings(&mut self, settings: FogRelevanceSettings) {
        self.lead_rules = widened_sight(&self.rules, settings.lead_margin_m());
        self.settings = settings;
    }

    /// True when the last prepared tick shows `entity` to `team`. Fog off and observer teams see
    /// everything; a player team that no connection maps to was not prepared and sees nothing.
    pub fn shown_to_team(&self, team: TeamId, entity: Entity) -> bool {
        if !self.rules.enabled || !self.rules.is_player_team(team) {
            return true;
        }
        self.teams.get(&team).is_some_and(|t| t.shown.contains(&entity))
    }

    /// The filter's answer for `peer` (team from `team`), counting it.
    fn relevant_to(&self, world: &World, peer: PeerId, team: Option<TeamId>, entity: Entity) -> bool {
        if !self.rules.enabled {
            return true;
        }
        let Some(team) = team else {
            self.unmapped.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        if !self.rules.is_player_team(team) {
            return true;
        }
        let shown = self.teams.get(&team).is_some_and(|t| t.shown.contains(&entity));
        if let Some(c) = self.peers.get(&peer) {
            c.evaluated.fetch_add(1, Ordering::Relaxed);
            if !shown {
                c.filtered.fetch_add(1, Ordering::Relaxed);
            }
        }
        if !shown && world.get::<FogUnit>(entity).is_none() && world.get::<FogEffect>(entity).is_none() {
            self.unlisted.fetch_add(1, Ordering::Relaxed);
        }
        shown
    }

    /// Whether a connection on `team` may receive an event placed at `place`, by the last prepared
    /// tick (see [`fog_event_shown`]). `EventPlace::Entity` carries the server entity's
    /// `Entity::to_bits()`.
    pub fn event_shown(&self, team: Option<TeamId>, place: EventPlace) -> bool {
        let shown = self.event_shown_uncounted(team, place);
        let counter = if shown {
            &self.events_passed
        } else {
            &self.events_dropped
        };
        counter.fetch_add(1, Ordering::Relaxed);
        shown
    }

    fn event_shown_uncounted(&self, team: Option<TeamId>, place: EventPlace) -> bool {
        if self.settings.events == FogEventPolicy::PassAll || !self.rules.enabled {
            return true;
        }
        let Some(team) = team else {
            return false;
        };
        if !self.rules.is_player_team(team) {
            return true;
        }
        let Some(fog) = self.teams.get(&team) else {
            return false;
        };
        match place {
            EventPlace::Unplaced => true,
            EventPlace::Entity(bits) => Entity::try_from_bits(bits).is_some_and(|e| fog.shown.contains(&e)),
            EventPlace::At(p) => fog.vision.sees_point([p.x, p.y, p.z], &*self.los),
        }
    }

    /// The agent's view: settings, the last tick's per-team counts and the running per-peer totals.
    pub fn status(&self) -> FogRelevanceStatus {
        FogRelevanceStatus {
            enabled: self.rules.enabled,
            team_count: self.rules.team_count,
            linger_ticks: self.settings.linger_ticks,
            lead_ticks: self.settings.lead_ticks,
            lead_margin_m: self.settings.lead_margin_m(),
            event_policy: match self.settings.events {
                FogEventPolicy::HideUnseen => "hide_unseen",
                FogEventPolicy::PassAll => "pass_all",
            },
            line_of_sight: self.los_name.clone(),
            ticks: self.ticks,
            units: self.units,
            unplaced: self.unplaced,
            effects: self.effects,
            visions_built: self.visions_built,
            lead_visions_built: self.lead_visions_built,
            visions_total: self.visions_total,
            teams: self.teams.values().map(|t| t.stats).collect(),
            peers: self
                .peers
                .iter()
                .map(|(peer, c)| PeerFogStats {
                    peer: *peer,
                    team: c.team,
                    evaluated: c.evaluated.load(Ordering::Relaxed),
                    filtered: c.filtered.load(Ordering::Relaxed),
                })
                .collect(),
            unmapped_refusals: self.unmapped.load(Ordering::Relaxed),
            unlisted_refusals: self.unlisted.load(Ordering::Relaxed),
            linger_hits_total: self.linger_hits_total,
            lead_hits_total: self.lead_hits_total,
            events_passed: self.events_passed.load(Ordering::Relaxed),
            events_dropped: self.events_dropped.load(Ordering::Relaxed),
        }
    }
}

/// Whether the server may send `peer` an event placed at `place` (a hit marker, a muzzle flash, a
/// sound cue), by the last prepared tick: under [`FogEventPolicy::HideUnseen`] an event placed on
/// an entity hidden from the peer's team, or at a point outside its sight, is refused; unplaced
/// events pass. `EventPlace::Entity` carries the server entity's `Entity::to_bits()`. Without a
/// [`FogRelevance`] every event passes; a peer with no team is refused every placed event.
pub fn fog_event_shown(world: &World, peer: PeerId, place: EventPlace) -> bool {
    let Some(fog) = world.get_resource::<FogRelevance>() else {
        return true;
    };
    let team = world.get_resource::<FogTeamOf>().and_then(|t| t.get(peer));
    fog.event_shown(team, place)
}

/// The net relevance filter registered as [`FOG_RELEVANCE_FILTER`]: a set lookup in the team's
/// prepared shown set. Without a [`FogRelevance`] it refuses (fail closed).
pub struct FogRelevanceFilter;

impl NetRelevanceFilter for FogRelevanceFilter {
    fn relevant(world: &World, entity: Entity, peer: PeerId) -> bool {
        let Some(fog) = world.get_resource::<FogRelevance>() else {
            return false;
        };
        let team = world.get_resource::<FogTeamOf>().and_then(|t| t.get(peer));
        fog.relevant_to(world, peer, team, entity)
    }
}

/// Inserts `fog` and an empty [`FogTeamOf`] (unless present), registers [`FogRelevanceFilter`] as
/// [`FOG_RELEVANCE_FILTER`], and adds [`fog_relevance_profile`] as [`FOG_RELEVANCE_PROFILE`] when the
/// world has `NetProfiles` and no profile of that name (call it after
/// `regia_net::install_net_interest`). The caller schedules [`fog_relevance_prepare`] once per
/// server tick before the send ([`FogRelevancePlugin`] does).
pub fn install_fog_relevance(world: &mut World, fog: FogRelevance) {
    world.insert_resource(fog);
    world.init_resource::<FogTeamOf>();
    add_net_relevance_filter::<FogRelevanceFilter>(world, FOG_RELEVANCE_FILTER);
    if let Some(mut profiles) = world.get_resource_mut::<NetProfiles>()
        && profiles.get(FOG_RELEVANCE_PROFILE).is_none()
        && let Err(err) = profiles.insert(FOG_RELEVANCE_PROFILE, fog_relevance_profile())
    {
        tracing::error!(target: "regia_framework", %err, "fog relevance profile refused");
    }
}

/// Adds the fog relevance to a server app: [`install_fog_relevance`] plus [`fog_relevance_prepare`]
/// in `FixedUpdate`, in `NetSet::Gather` (after gameplay and prediction, before the send), on
/// servers only.
#[derive(Clone)]
pub struct FogRelevancePlugin {
    /// The fog rules.
    pub rules: FogRules,
    /// The grace settings.
    pub settings: FogRelevanceSettings,
    /// The line-of-sight hook and its name (`None` is open ground).
    pub line_of_sight: Option<(String, Arc<dyn LineOfSight + Send + Sync>)>,
}

impl FogRelevancePlugin {
    /// The plugin over open ground.
    pub fn new(rules: FogRules, settings: FogRelevanceSettings) -> Self {
        Self {
            rules,
            settings,
            line_of_sight: None,
        }
    }
}

impl Plugin for FogRelevancePlugin {
    fn build(&self, app: &mut App) {
        let mut fog = FogRelevance::new(self.rules.clone(), self.settings.clone());
        if let Some((name, los)) = &self.line_of_sight {
            fog.set_line_of_sight(name.clone(), Arc::clone(los));
        }
        install_fog_relevance(app.world_mut(), fog);
        app.add_systems(
            FixedUpdate,
            fog_relevance_prepare.in_set(NetSet::Gather).run_if(on_server()),
        );
    }

    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }
}

/// The server's fogged entities as a [`FogWorld`] for one tick.
struct FogRows<'a> {
    rows: &'a [FogEntity<'a>],
    settings: &'a FogRelevanceSettings,
}

impl FogWorld for FogRows<'_> {
    fn fog_entities(&self) -> impl Iterator<Item = FogEntity<'_>> {
        self.rows.iter().copied()
    }

    fn is_allied(&self, a: TeamId, b: TeamId) -> bool {
        self.settings.allied(a, b)
    }
}

fn world_position(t: Option<&Transform>, g: Option<&GlobalTransform>, child: bool) -> Option<Vec3> {
    if child && let Some(g) = g {
        return Some(g.0.translation.into());
    }
    t.map(|t| t.translation)
}

/// Fogged entities, read once per tick.
type FogUnitQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static FogUnit,
        Option<&'static Transform>,
        Option<&'static GlobalTransform>,
        Has<ChildOf>,
    ),
>;

/// Effects, read once per tick.
type FogEffectQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static FogEffect,
        Option<&'static Transform>,
        Option<&'static GlobalTransform>,
        Has<ChildOf>,
    ),
>;

/// Once per server tick, before the send: builds one sight vision (and, with the lead on, one lead
/// vision) per player team some connection maps to, applies linger and lead, and stores each team's
/// shown set for [`FogRelevanceFilter`]. Teams no connection maps to are dropped with their linger
/// state.
pub fn fog_relevance_prepare(
    fog: Option<ResMut<FogRelevance>>,
    teams_of: Option<Res<FogTeamOf>>,
    units: FogUnitQuery,
    effects: FogEffectQuery,
) {
    let Some(mut fog) = fog else {
        return;
    };
    let _span = tracing::trace_span!(target: "regia_framework", "fog_relevance_prepare").entered();
    let fog = &mut *fog;
    fog.ticks += 1;
    let tick = fog.ticks;
    // Teams and connections.
    let mut team_peers: BTreeMap<TeamId, u64> = BTreeMap::new();
    let mut mapped: BTreeMap<PeerId, TeamId> = BTreeMap::new();
    if let Some(teams_of) = &teams_of {
        for (peer, team) in teams_of.iter() {
            mapped.insert(peer, team);
            if fog.rules.is_player_team(team) {
                *team_peers.entry(team).or_insert(0) += 1;
            }
        }
    }
    fog.peers.retain(|p, _| mapped.contains_key(p));
    for (peer, team) in &mapped {
        fog.peers.entry(*peer).or_default().team = *team;
    }
    fog.teams.retain(|t, _| team_peers.contains_key(t));
    // Rows.
    let mut entities: Vec<Entity> = Vec::new();
    let mut rows: Vec<FogEntity<'_>> = Vec::new();
    let mut unplaced = 0u64;
    for (entity, unit, t, g, child) in &units {
        let Some(p) = world_position(t, g, child) else {
            unplaced += 1;
            continue;
        };
        let mut row = FogEntity::new(entity.to_bits(), &unit.kind, unit.class, unit.team, p.to_array());
        row.alive = unit.alive;
        entities.push(entity);
        rows.push(row);
    }
    let effect_rows: Vec<(Entity, Option<Entity>, Option<Vec3>)> = effects
        .iter()
        .map(|(e, fx, t, g, child)| (e, fx.source, world_position(t, g, child)))
        .collect();
    fog.units = rows.len() as u64;
    fog.unplaced = unplaced;
    fog.effects = effect_rows.len() as u64;
    let margin = fog.settings.lead_margin_m();
    let FogRelevance {
        rules,
        lead_rules,
        settings,
        los,
        teams,
        ..
    } = &mut *fog;
    let (rules, lead_rules, settings): (&FogRules, &FogRules, &FogRelevanceSettings) =
        (rules, lead_rules, settings);
    let world = FogRows {
        rows: &rows,
        settings,
    };
    let los: &(dyn LineOfSight + Send + Sync) = &**los;
    let (mut built, mut lead_built, mut linger_total, mut lead_total) = (0u64, 0u64, 0u64, 0u64);
    for (team, peers) in &team_peers {
        let team = *team;
        let tf = teams.entry(team).or_default();
        tf.vision.rebuild(&world, rules, team);
        built += 1;
        let lead_on = margin > 0.0;
        if lead_on {
            tf.lead.rebuild(&world, lead_rules, team);
            lead_built += 1;
        }
        let mut counters = FogCounters::default();
        let mut stats = TeamFogStats {
            team,
            peers: *peers,
            watchers: tf.vision.watcher_count() as u64,
            ..TeamFogStats::default()
        };
        tf.shown.clear();
        for (entity, row) in entities.iter().zip(&rows) {
            let seen = tf.vision.entity_visible_counted(&world, row, los, &mut counters);
            let lead = !seen && lead_on && tf.lead.entity_visible_counted(&world, row, los, &mut counters);
            if seen || lead {
                tf.linger.insert(*entity, (settings.linger_ticks, tick));
                tf.shown.insert(*entity);
                if lead {
                    stats.lead_hits += 1;
                }
                continue;
            }
            match tf.linger.get_mut(entity) {
                Some((left, listed)) if *left > 0 => {
                    *left -= 1;
                    *listed = tick;
                    tf.shown.insert(*entity);
                    stats.linger_hits += 1;
                }
                _ => {
                    tf.linger.remove(entity);
                    stats.hidden += 1;
                }
            }
        }
        // Despawned or unplaced entities forget their linger.
        tf.linger.retain(|_, (_, listed)| *listed == tick);
        stats.shown = tf.shown.len() as u64;
        for (entity, source, at) in &effect_rows {
            let shown = settings.events == FogEventPolicy::PassAll
                || !rules.enabled
                || source.is_some_and(|s| tf.shown.contains(&s))
                || at.is_some_and(|p| tf.vision.sees_point_counted(p.to_array(), los, &mut counters));
            if shown {
                tf.shown.insert(*entity);
                stats.effects_shown += 1;
            } else {
                stats.effects_hidden += 1;
            }
        }
        stats.queries = counters.queries;
        linger_total += stats.linger_hits;
        lead_total += stats.lead_hits;
        tf.stats = stats;
    }
    fog.visions_built = built;
    fog.lead_visions_built = lead_built;
    fog.visions_total += built;
    fog.linger_hits_total += linger_total;
    fog.lead_hits_total += lead_total;
}
