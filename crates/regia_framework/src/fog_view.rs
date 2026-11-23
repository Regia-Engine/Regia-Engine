// SPDX-License-Identifier: MPL-2.0
//! Per-viewer fog of war for the sim bridge (plan row S-16, ADR-8154).
//!
//! [`FogViewSim`] wraps an [`ExternalSim`] whose snapshot also reports fog rows
//! ([`regia_detsim::fog::FogWorld`]). Every frame it reads the full snapshot into a scratch value,
//! works out what one viewing team sees ([`regia_detsim::fog`], a pure function of that state),
//! and hands the bridge a [`ViewerSnapshot`] holding only the visible entities and their extras.
//! A hidden unit therefore never reaches the presentation world: no entity, no transform, no
//! extras, and (with [`FogEventPolicy::HideUnseen`]) no event placed on it or at an unseen point.
//!
//! The sim itself is untouched: it keeps stepping the full state, so lockstep peers stay in sync
//! whatever each player sees. A server-authoritative game decides what each client is sent with
//! `fog_relevance` (feature `net`): the same rules as a net relevance filter, so a hidden enemy is
//! never replicated to the other team.
//!
//! Setup: wrap the sim in `FogViewSim::new(sim, rules, viewer)`, give that to
//! [`SimBridgePlugin`](crate::sim_bridge::SimBridgePlugin), and add `FogViewPlugin::<YourSim>` for
//! the agent surface (`fog.status`, `fog.set_viewer`). The sim's snapshot type implements
//! [`FogWorld`] next to [`SimSnapshot`]: one fog row per entity (id, kind, [`SightClass`], team,
//! position, alive) and the alliance query.
//!
//! A line-of-sight query (terrain, walls) plugs in with [`FogViewSim::with_line_of_sight`]; the
//! default is open ground.
//!
//! For presentation (a fog overlay, a minimap layer), give the view a [`FogGrid`] with
//! [`FogViewSim::with_grid`]: every filtered snapshot updates it for the viewer (unexplored,
//! explored, visible cells and their dirty rows), and [`crate::fog_texture`] turns it into the
//! texture the overlay and the minimap draw.

use std::marker::PhantomData;
use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
pub use regia_detsim::fog::{CellState, FogGrid};
pub use regia_detsim::fog::{
    ElevationRule, FogEntity, FogRules, FogWorld, Ghost, KindSight, SightClass, SightTable, TeamId,
};
use regia_detsim::fog::{FogCounters, FogMemory, TeamVision};
pub use regia_detsim::los::{LineOfSight, OpenGround};

use crate::sim_bridge::{BridgeInput, EventPlace, ExternalSim, SimEntityView, SimSnapshot, SnapshotTick};

/// One visible entity of a [`ViewerSnapshot`]: an owned copy of the sim's row.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewerEntity {
    /// The sim's stable id.
    pub id: u64,
    /// The kind key.
    pub kind: String,
    /// The owning team.
    pub team: u32,
    /// World position in engine metres.
    pub translation: Vec3,
    /// World rotation.
    pub rotation: Quat,
}

/// A remembered structure the viewer does not see now: its last sighting, never live data (see
/// [`regia_detsim::fog::FogMemory`]). A presentation layer draws it as a ghost on explored ground.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewerGhost {
    /// The sim's stable id.
    pub id: u64,
    /// The kind key when last seen.
    pub kind: String,
    /// The team when last seen.
    pub team: u32,
    /// Where it was last seen, in engine metres.
    pub translation: Vec3,
    /// The sim tick of the last sighting.
    pub tick: u64,
}

/// What one viewer may see of a sim snapshot: the visible entities, in the sim's order, and their
/// extras, plus the ghosts of remembered structures it does not see now. Holds no live data about
/// hidden entities.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ViewerSnapshot {
    entities: Vec<ViewerEntity>,
    /// `(id, extras)`, sorted by id.
    extras: Vec<(u64, Vec<(String, f32)>)>,
    /// Ghosts of remembered, currently hidden structures, sorted by id.
    ghosts: Vec<ViewerGhost>,
}

impl ViewerSnapshot {
    /// The visible entities, in the order the sim listed them.
    pub fn entities(&self) -> &[ViewerEntity] {
        &self.entities
    }

    /// Number of visible entities.
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// True when nothing is visible.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// True when entity `id` is in this viewer's snapshot.
    pub fn contains(&self, id: u64) -> bool {
        self.entities.iter().any(|e| e.id == id)
    }

    /// Ghosts of remembered structures the viewer does not see now, sorted by id. They are not
    /// spawned as sim entities; a fog presentation draws them.
    pub fn ghosts(&self) -> &[ViewerGhost] {
        &self.ghosts
    }

    /// Empties the snapshot, keeping its buffers.
    pub fn clear(&mut self) {
        self.entities.clear();
        self.extras.clear();
        self.ghosts.clear();
    }
}

impl SimSnapshot for ViewerSnapshot {
    fn for_each_entity(&self, mut f: impl FnMut(SimEntityView<'_>)) {
        for e in &self.entities {
            f(SimEntityView {
                id: e.id,
                kind: &e.kind,
                team: e.team,
                translation: e.translation,
                rotation: e.rotation,
            });
        }
    }

    fn extras_for(&self, id: u64, out: &mut Vec<(String, f32)>) {
        if let Ok(i) = self.extras.binary_search_by_key(&id, |(eid, _)| *eid) {
            out.extend(self.extras[i].1.iter().cloned());
        }
    }
}

/// What [`FogViewSim`] does with the events the sim drains.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FogEventPolicy {
    /// Drop an event placed on an entity the viewer does not see this tick, or at a point outside
    /// the viewer's sight. Unplaced events pass. The default: an explosion on a hidden unit would
    /// otherwise give its position away. A sim should report a destroyed entity as dead (not
    /// remove it) on the tick it dies, so the events of its death show (the dead always show).
    #[default]
    HideUnseen,
    /// Pass every event (a game that filters its own events).
    PassAll,
}

/// Counters of the last filtered frame, and running event totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FogViewStats {
    /// Snapshots filtered so far.
    pub frames: u64,
    /// The sim tick of the last snapshot.
    pub tick: u64,
    /// Entities the viewer saw in the last snapshot.
    pub visible: u64,
    /// Entities held back from the viewer in the last snapshot (including unlisted ones).
    pub hidden: u64,
    /// Snapshot entities with no fog row in the last snapshot: held back (fail closed).
    pub unlisted: u64,
    /// Living own or allied watchers in the last snapshot.
    pub watchers: u64,
    /// Events passed to the viewer so far.
    pub events_passed: u64,
    /// Events dropped so far because they were placed on a hidden entity or an unseen point.
    pub events_dropped: u64,
    /// Living own or allied detectors in the last snapshot.
    pub detectors: u64,
    /// Sight queries of the last snapshot (entities and ghost spots answered from sight).
    pub queries: u64,
    /// Watchers the last snapshot's queries tested exactly (spatial-index candidates).
    pub index_candidates: u64,
    /// Line-of-sight hook calls of the last snapshot.
    pub los_calls: u64,
    /// Ghosts in the last snapshot (remembered structures the viewer does not see now).
    pub ghosts: u64,
    /// Ghosts dropped in the last snapshot (their spot seen empty, or seen dead).
    pub ghosts_dropped: u64,
}

/// An [`ExternalSim`] that shows one viewing team only what it sees (see the module docs).
#[derive(Resource)]
pub struct FogViewSim<S: ExternalSim + Sync> {
    inner: S,
    viewer: TeamId,
    rules: FogRules,
    los: Box<dyn LineOfSight + Send + Sync>,
    los_name: String,
    events: FogEventPolicy,
    vision: TeamVision,
    memory: FogMemory,
    /// When true, the snapshot keeps every entity. The game sets this from
    /// `regia_rules::vision::reveals_every_team` (a finished match, or a neutral seat the host
    /// granted). False keeps the team filter.
    reveal_all: bool,
    /// Visible fog ids of the last snapshot, sorted.
    visible: Vec<u64>,
    stats: FogViewStats,
    pending: Vec<S::Event>,
    extras: Vec<(String, f32)>,
    /// Fog ids of the last snapshot, sorted (scratch).
    listed: Vec<u64>,
    /// The viewer's presentation grid, when one was given.
    grid: Option<FogGrid>,
    /// [`Self::with_explored_per_team`]: every other player team's grid, by team.
    team_grids: Option<std::collections::BTreeMap<TeamId, FogGrid>>,
    /// Scratch vision for the other teams' grids.
    other_vision: TeamVision,
    /// [`Self::with_cell_ground`]: the ground under a grid cell, for the elevation rule.
    cell_ground: Option<Box<dyn Fn(f32, f32) -> f32 + Send + Sync>>,
}

impl<S: ExternalSim + Sync> FogViewSim<S>
where
    S::Snapshot: FogWorld,
{
    /// Wraps `inner`, showing team `viewer` what it sees under `rules`, over open ground.
    pub fn new(inner: S, rules: FogRules, viewer: TeamId) -> Self {
        Self {
            inner,
            viewer,
            rules,
            los: Box::new(OpenGround),
            los_name: "open_ground".to_string(),
            events: FogEventPolicy::default(),
            vision: TeamVision::default(),
            memory: FogMemory::new(),
            reveal_all: false,
            visible: Vec::new(),
            stats: FogViewStats::default(),
            pending: Vec::new(),
            extras: Vec::new(),
            listed: Vec::new(),
            grid: None,
            team_grids: None,
            other_vision: TeamVision::default(),
            cell_ground: None,
        }
    }

    /// Keeps the explored ground of every player team, not only the viewer's (KI-GP-018,
    /// ADR-10070): with a grid ([`Self::with_grid`]) each filtered snapshot also updates one grid per
    /// other player team, and [`Self::set_viewer`] switches to that team's grid (its explored layer
    /// kept) instead of starting an unexplored one. Costs one vision rebuild and grid update per
    /// player team per snapshot.
    pub fn with_explored_per_team(mut self) -> Self {
        self.team_grids = Some(std::collections::BTreeMap::new());
        self
    }

    /// The ground height (metres) at a world `(x, z)`, so the grid applies the elevation rule
    /// ([`FogRules::elevation`]) to its cells (KI-GP-018, ADR-10070; `FogGrid::update_on_ground`).
    /// Without it a cell has no height and the rule applies to entities only.
    pub fn with_cell_ground(mut self, ground: impl Fn(f32, f32) -> f32 + Send + Sync + 'static) -> Self {
        self.cell_ground = Some(Box::new(ground));
        self
    }

    /// The explored grid kept for player team `team` ([`Self::with_explored_per_team`]): the
    /// viewer's own grid for the viewer, `None` for a team not tracked (yet).
    pub fn team_grid(&self, team: TeamId) -> Option<&FogGrid> {
        if team == self.viewer {
            return self.grid.as_ref();
        }
        self.team_grids.as_ref()?.get(&team)
    }

    /// Keeps `grid` up to date for the viewer: every filtered snapshot rebuilds its visible layer
    /// (through the same line-of-sight hook) and adds to its explored layer. With fog off the
    /// grid shows every cell visible. A new viewer starts from an unexplored grid.
    pub fn with_grid(mut self, grid: FogGrid) -> Self {
        self.grid = Some(grid);
        self
    }

    /// The viewer's presentation grid ([`Self::with_grid`]).
    pub fn grid(&self) -> Option<&FogGrid> {
        self.grid.as_ref()
    }

    /// The viewer's presentation grid, mutable (a fog texture clears its dirty rows).
    pub fn grid_mut(&mut self) -> Option<&mut FogGrid> {
        self.grid.as_mut()
    }

    /// The living own and allied watchers of the last filtered snapshot: `(position, sight
    /// radius)` (the editor's fog view draws them).
    pub fn watcher_circles(&self) -> impl Iterator<Item = ([f32; 3], f32)> + '_ {
        self.vision.watcher_circles()
    }

    /// Uses `los` for line of sight; `name` is what the agent's `fog.status` reports.
    pub fn with_line_of_sight(
        mut self,
        name: impl Into<String>,
        los: impl LineOfSight + Send + Sync + 'static,
    ) -> Self {
        self.los = Box::new(los);
        self.los_name = name.into();
        self
    }

    /// Sets what happens to drained events.
    pub fn with_event_policy(mut self, policy: FogEventPolicy) -> Self {
        self.events = policy;
        self
    }

    /// The viewing team. Team 0 (or a team above [`FogRules::team_count`]) observes everything.
    pub fn viewer(&self) -> TeamId {
        self.viewer
    }

    /// Switches the viewing team; the next snapshot uses it.
    pub fn set_viewer(&mut self, viewer: TeamId) {
        if viewer != self.viewer
            && let Some(grid) = self.grid.as_mut()
        {
            match self.team_grids.as_mut() {
                // Keep the old viewer's grid and take the new one's (KI-GP-018).
                Some(store) => {
                    let mut next = store.remove(&viewer).unwrap_or_else(|| {
                        let mut fresh = grid.clone();
                        fresh.clear();
                        fresh
                    });
                    next.mark_all_dirty();
                    let old = std::mem::replace(grid, next);
                    if self.rules.is_player_team(self.viewer) {
                        store.insert(self.viewer, old);
                    }
                }
                None => grid.clear(),
            }
        }
        self.viewer = viewer;
    }

    /// Show every entity on the next snapshots when `reveal_all` is true.
    ///
    /// The game passes `regia_rules::vision::reveals_every_team`. False leaves the team filter
    /// unchanged.
    pub fn set_reveal_all(&mut self, reveal_all: bool) {
        self.reveal_all = reveal_all;
    }

    /// The fog rules.
    pub fn rules(&self) -> &FogRules {
        &self.rules
    }

    /// Mutable fog rules (fog on/off, team count, sight table); the next snapshot uses them.
    pub fn rules_mut(&mut self) -> &mut FogRules {
        &mut self.rules
    }

    /// The event policy.
    pub fn event_policy(&self) -> FogEventPolicy {
        self.events
    }

    /// The line-of-sight hook's name.
    pub fn line_of_sight_name(&self) -> &str {
        &self.los_name
    }

    /// The wrapped sim.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// The wrapped sim, mutably.
    pub fn inner_mut(&mut self) -> &mut S {
        &mut self.inner
    }

    /// Counters of the last filtered frame.
    pub fn stats(&self) -> FogViewStats {
        self.stats
    }

    /// The ghost memory of every team this view has shown (remembered kinds only).
    pub fn memory(&self) -> &FogMemory {
        &self.memory
    }

    /// Forgets every ghost (a new match, a reconnect).
    pub fn clear_memory(&mut self) {
        self.memory.clear();
    }

    /// Copies the entities of `full` that the viewer sees into `out` (cleared first), updates the
    /// ghost memory (ghosts stamped with the tick of the last [`ExternalSim::read_snapshot`]) and
    /// the counters. This is the whole filter; [`ExternalSim::read_snapshot`] calls it.
    pub fn filter_into(&mut self, full: &S::Snapshot, out: &mut ViewerSnapshot) {
        let _span = tracing::trace_span!(target: "regia_framework", "fog_view_filter").entered();
        out.clear();
        self.vision.rebuild(full, &self.rules, self.viewer);
        if let Some(grid) = self.grid.as_mut() {
            if self.vision.is_enabled() {
                match self.cell_ground.as_deref() {
                    Some(ground) => grid.update_on_ground(&self.vision, &*self.los, ground),
                    None => grid.update(&self.vision, &*self.los),
                };
                if let Some(store) = self.team_grids.as_mut() {
                    let _span =
                        tracing::trace_span!(target: "regia_framework", "fog_view_team_grids").entered();
                    for team in 1..=self.rules.team_count {
                        if team == self.viewer {
                            continue;
                        }
                        let other = store.entry(team).or_insert_with(|| {
                            let mut fresh = grid.clone();
                            fresh.clear();
                            fresh
                        });
                        self.other_vision.rebuild(full, &self.rules, team);
                        match self.cell_ground.as_deref() {
                            Some(ground) => other.update_on_ground(&self.other_vision, &*self.los, ground),
                            None => other.update(&self.other_vision, &*self.los),
                        };
                    }
                }
            } else if grid.visible.contains(&0) || grid.seen.contains(&0) {
                // Fog off shows everything.
                grid.visible.fill(1);
                grid.seen.fill(1);
                grid.mark_all_dirty();
            }
        }
        self.visible.clear();
        let listed = &mut self.listed;
        listed.clear();
        let mut counters = FogCounters::default();
        for entity in full.fog_entities() {
            listed.push(entity.id);
            if self.reveal_all
                || self
                    .vision
                    .entity_visible_counted(full, &entity, &*self.los, &mut counters)
            {
                self.visible.push(entity.id);
            }
        }
        let memory = self
            .memory
            .update(full, &self.vision, &*self.los, self.stats.tick);
        self.visible.sort_unstable();
        self.visible.dedup();
        listed.sort_unstable();
        let (mut hidden, mut unlisted) = (0u64, 0u64);
        let visible = &self.visible;
        let listed = &self.listed;
        full.for_each_entity(|view| {
            if visible.binary_search(&view.id).is_ok() {
                out.entities.push(ViewerEntity {
                    id: view.id,
                    kind: view.kind.to_string(),
                    team: view.team,
                    translation: view.translation,
                    rotation: view.rotation,
                });
            } else {
                hidden += 1;
                if listed.binary_search(&view.id).is_err() {
                    unlisted += 1;
                }
            }
        });
        for i in 0..out.entities.len() {
            let id = out.entities[i].id;
            self.extras.clear();
            full.extras_for(id, &mut self.extras);
            if !self.extras.is_empty() {
                out.extras.push((id, self.extras.clone()));
            }
        }
        out.extras.sort_by_key(|(id, _)| *id);
        out.extras.dedup_by_key(|(id, _)| *id);
        // Ghosts: only remembered fields, and only of entities the viewer does not see now.
        for ghost in self.memory.ghosts(self.viewer) {
            if self.visible.binary_search(&ghost.id).is_ok() {
                continue;
            }
            out.ghosts.push(ViewerGhost {
                id: ghost.id,
                kind: ghost.kind.clone(),
                team: ghost.team,
                translation: Vec3::from_array(ghost.position),
                tick: ghost.tick,
            });
        }
        self.stats.frames += 1;
        self.stats.visible = out.entities.len() as u64;
        self.stats.hidden = hidden;
        self.stats.unlisted = unlisted;
        self.stats.watchers = self.vision.watcher_count() as u64;
        self.stats.detectors = self.vision.detector_count() as u64;
        self.stats.queries = counters.queries + memory.queries;
        self.stats.index_candidates = counters.index_candidates;
        self.stats.los_calls = counters.los_calls;
        self.stats.ghosts = out.ghosts.len() as u64;
        self.stats.ghosts_dropped = memory.dropped;
    }

    /// True when the viewer may receive an event placed at `place` (by the last filtered frame).
    fn event_shown(&self, place: EventPlace) -> bool {
        if self.events == FogEventPolicy::PassAll || !self.rules.enabled || self.vision.is_observer() {
            return true;
        }
        match place {
            EventPlace::Unplaced => true,
            EventPlace::Entity(id) => self.visible.binary_search(&id).is_ok(),
            EventPlace::At(p) => self.vision.sees_point([p.x, p.y, p.z], &*self.los),
        }
    }
}

impl<S: ExternalSim + Sync> ExternalSim for FogViewSim<S>
where
    S::Snapshot: FogWorld,
{
    type Snapshot = ViewerSnapshot;
    type Event = S::Event;

    fn submit_input(&mut self, input: &BridgeInput) {
        self.inner.submit_input(input);
    }

    fn read_snapshot(&mut self, frame_time: Duration, out: &mut ViewerSnapshot) -> SnapshotTick {
        let mut full = S::Snapshot::default();
        let tick = self.inner.read_snapshot(frame_time, &mut full);
        self.stats.tick = tick.0;
        self.filter_into(&full, out);
        tick
    }

    fn drain_events(&mut self, out: &mut Vec<Self::Event>) {
        let mut pending = std::mem::take(&mut self.pending);
        self.inner.drain_events(&mut pending);
        for event in pending.drain(..) {
            if self.event_shown(S::event_place(&event)) {
                self.stats.events_passed += 1;
                out.push(event);
            } else {
                self.stats.events_dropped += 1;
            }
        }
        // Keep the buffer for the next frame.
        self.pending = pending;
    }

    fn event_kind(event: &Self::Event) -> String {
        S::event_kind(event)
    }

    fn event_place(event: &Self::Event) -> EventPlace {
        S::event_place(event)
    }
}

/// `rules` with every sight and detection radius widened by `margin_m` metres: the class defaults,
/// each kind row's `radius_m` and each detector's `detector_m`. A radius of 0 (a kind that never
/// watches, a row that detects nothing) stays 0; a margin that is not a positive finite number
/// changes nothing. Fog on/off, the team count, eye heights and the elevation rule are kept.
///
/// A [`TeamVision`] built from the widened rules answers "inside sight plus the margin": the
/// server's fog relevance (`fog_relevance`, feature `net`) uses it to replicate an enemy a few ticks
/// before it comes into sight.
pub fn widened_sight(rules: &FogRules, margin_m: f32) -> FogRules {
    let mut out = rules.clone();
    if !(margin_m.is_finite() && margin_m > 0.0) {
        return out;
    }
    let widen = |r: &mut f32| {
        if *r > 0.0 {
            *r += margin_m;
        }
    };
    widen(&mut out.sight.unit_m);
    widen(&mut out.sight.on_foot_m);
    widen(&mut out.sight.building_m);
    for row in &mut out.sight.kinds {
        widen(&mut row.radius_m);
        widen(&mut row.detector_m);
    }
    out
}

/// The fog view's state for the agent (`fog.status`), copied from the [`FogViewSim`] every frame
/// by [`FogViewPlugin`].
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct FogViewStatus {
    /// Fog on.
    pub enabled: bool,
    /// The viewing team.
    pub viewer: TeamId,
    /// True when the viewer observes everything (team 0 or above the team count).
    pub observer: bool,
    /// Player teams are `1..=team_count`.
    pub team_count: TeamId,
    /// Kind rows in the sight table.
    pub sight_kinds: usize,
    /// The line-of-sight hook's name.
    pub line_of_sight: String,
    /// The event policy (`hide_unseen` or `pass_all`).
    pub event_policy: &'static str,
    /// Counters of the last filtered frame.
    pub stats: FogViewStats,
}

/// A request to change the fog view, applied at the start of the next frame (the agent's
/// `fog.set_viewer` writes it).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FogViewControl {
    /// Switch to this viewing team.
    pub viewer: Option<TeamId>,
    /// Turn fog on or off.
    pub enabled: Option<bool>,
}

/// Publishes a [`FogViewSim<S>`]'s state as [`FogViewStatus`] and applies [`FogViewControl`]
/// requests. Add it next to `SimBridgePlugin::new(FogViewSim::new(..))`.
pub struct FogViewPlugin<S>(PhantomData<fn() -> S>);

impl<S> Default for FogViewPlugin<S> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<S: ExternalSim + Sync> Plugin for FogViewPlugin<S>
where
    S::Snapshot: FogWorld,
{
    fn build(&self, app: &mut App) {
        app.init_resource::<FogViewStatus>()
            .init_resource::<FogViewControl>()
            .add_systems(First, apply_fog_control::<S>)
            .add_systems(Update, publish_fog_status::<S>);
    }

    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }
}

fn apply_fog_control<S: ExternalSim + Sync>(
    mut control: ResMut<FogViewControl>,
    sim: Option<ResMut<FogViewSim<S>>>,
) where
    S::Snapshot: FogWorld,
{
    if control.viewer.is_none() && control.enabled.is_none() {
        return;
    }
    let Some(mut sim) = sim else {
        return;
    };
    if let Some(viewer) = control.viewer.take() {
        sim.set_viewer(viewer);
    }
    if let Some(enabled) = control.enabled.take() {
        sim.rules_mut().enabled = enabled;
    }
}

fn publish_fog_status<S: ExternalSim + Sync>(
    mut status: ResMut<FogViewStatus>,
    sim: Option<Res<FogViewSim<S>>>,
) where
    S::Snapshot: FogWorld,
{
    let Some(sim) = sim else {
        return;
    };
    let rules = sim.rules();
    if status.line_of_sight != sim.line_of_sight_name() {
        status.line_of_sight = sim.line_of_sight_name().to_string();
    }
    let line_of_sight = std::mem::take(&mut status.line_of_sight);
    *status = FogViewStatus {
        enabled: rules.enabled,
        viewer: sim.viewer(),
        observer: !rules.is_player_team(sim.viewer()),
        team_count: rules.team_count,
        sight_kinds: rules.sight.kinds.len(),
        line_of_sight,
        event_policy: match sim.event_policy() {
            FogEventPolicy::HideUnseen => "hide_unseen",
            FogEventPolicy::PassAll => "pass_all",
        },
        stats: sim.stats(),
    };
}
