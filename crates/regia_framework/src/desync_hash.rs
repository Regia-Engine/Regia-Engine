// SPDX-License-Identifier: MPL-2.0
//! Per-system lockstep desync hashes for ECS games (plan row F-05, ADR-8080).
//!
//! [`regia_detsim::hash`] keeps one hash per named system after every lockstep step, so a checksum
//! split names *which* system diverged and the first tick it did. A game outside the ECS writes its
//! own [`SystemFold`]. This module is the fold for an engine-native
//! game whose lockstep state lives in components and resources:
//!
//! - The game names its desync systems once (a `const` [`Systems`] list, usually one name per
//!   schedule set of its step) and tells [`EcsDesyncHash`] which `serde` components and resources each
//!   one owns ([`EcsDesyncHash::component`], [`EcsDesyncHash::resource`]).
//! - After every step, [`record_desync_hashes`] folds each system's columns into that system's
//!   [`Mix`]: every entity holding the component, **in entity-id order**, its id and then its value.
//!   The order never follows archetypes or table rows, so moving an entity between archetypes (adding
//!   and removing an unrelated marker) or spawning into archetypes in another order leaves the words
//!   alone; only values and ids move them.
//! - The vectors go into a [`SystemRing`] (240 ticks). The agent method `sim.desync_rows` reads the
//!   ring and, given another peer's rows, names the diverged systems ([`desync_rows`],
//!   [`compare_desync_rows`]).
//!
//! [`DesyncHashPlugin`] wires it: the record system runs in `FixedUpdate` (or the schedule the game
//! names) in [`DesyncHashSet`], after every schedule set the game registered with
//! [`DesyncHashPlugin::after`].
//!
//! **Diagnosis (ADR-8547).** [`EcsDesyncHash::diagnose_raw`] also keeps a raw-bits vector per row
//! ([`FloatRule::Raw`]), so a zero-sign or NaN-payload split is named at its tick
//! ([`diagnose_desync_rows`], [`Diagnosis::float_bits`]); the canonical words do not change. A row
//! whose fold hit a failing `Serialize` is flagged partial ([`DesyncRow::partial`]).
//!
//! **External sims.** A sim outside the ECS that implements
//! [`SystemFold`] records into a [`DesyncFeed`]; behind the threaded
//! sim host ([`ThreadedSimHost::start_with_desync`](crate::sim_host::ThreadedSimHost::start_with_desync))
//! the host thread records after every step and the host handle carries the rows, so
//! [`desync_rows`] (and `sim.desync_rows`) answer from it when the world has no [`EcsDesyncHash`].
//!
//! **Entity order.** By default the order key is the entity id, `(index << 32) | generation`
//! ([`entity_key`]): the same on two peers that spawn and despawn their lockstep entities in the same
//! order and spawn nothing else in that world. A game whose worlds also hold presentation entities
//! (cameras, UI, effects) that the peers spawn differently gives its lockstep entities an id component
//! and calls [`EcsDesyncHash::order_by`]: the key is then [`DesyncKey::desync_key`], and entities
//! without the id are left out. A game with no such id calls [`EcsDesyncHash::order_by_content`]
//! instead: rows fold in the order of their own value words, so entity ids do not matter (ADR-10062).
//!
//! ```
//! use regia_app::prelude::*;
//! use regia_detsim::hash::Systems;
//! use regia_framework::desync_hash::{self, DesyncHashPlugin};
//! use serde::Serialize;
//!
//! #[derive(Component, Serialize)]
//! struct Position([f32; 2]);
//! #[derive(Resource, Serialize, Default)]
//! struct Gold(u32);
//!
//! const SYSTEMS: Systems<2> = Systems::new(["movement", "economy"]);
//!
//! let mut app = App::new();
//! app.add_plugins(MinimalPlugins).init_resource::<Gold>().add_plugins(
//!     DesyncHashPlugin::new(SYSTEMS)
//!         .component::<Position>("movement")
//!         .resource::<Gold>("economy")
//!         .in_schedule(Update),
//! );
//! app.world_mut().spawn(Position([1.0, 2.0]));
//! app.update();
//! let rows = desync_hash::desync_rows(app.world(), None, 8).expect("the plugin is installed");
//! assert_eq!(rows.systems, ["movement", "economy"]);
//! assert_eq!(rows.rows.len(), 1);
//! ```

use std::marker::PhantomData;
use std::sync::{Arc, Mutex, PoisonError};

use regia_app::prelude::*;
use regia_ecs::ecs::query::QueryState;
use regia_ecs::ecs::schedule::InternedSystemSet;
use serde::Serialize;

pub use regia_detsim::hash::{
    Diagnosis, DivergenceReport, FloatBitsSplit, FloatRule, Folded, MAX_TRACE_ROWS, Mix, RING_TICKS, RingRow,
    RowSide, SystemFold, SystemHashes, SystemRing, Systems, TraceRow, UnmatchedRow, Verdict, diagnose,
    diagnosis_lines, diverged_systems, divergence_lines,
};
use regia_detsim::hash::{finish_mixers, fold_system_hashes};

/// The set [`record_desync_hashes`] runs in. [`DesyncHashPlugin`] orders it after every set the
/// game registers with [`DesyncHashPlugin::after`]; a game may order it further itself.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DesyncHashSet;

/// The order key of a game's lockstep id component ([`EcsDesyncHash::order_by`]).
///
/// Two entities should not share a key; if they do, they fold in entity-id order among themselves.
pub trait DesyncKey: Component {
    /// The entity's key: the same on every peer for the same lockstep entity.
    fn desync_key(&self) -> u64;
}

/// The default order key: `(index << 32) | generation` of the entity id.
#[must_use]
pub fn entity_key(entity: Entity) -> u64 {
    (u64::from(entity.index_u32()) << 32) | u64::from(entity.generation().to_bits())
}

#[derive(Clone, Copy)]
enum Order {
    Entity,
    Key(fn(&World, Entity) -> Option<u64>),
    /// Rows in the order of their value words ([`EcsDesyncHash::order_by_content`], ADR-10062).
    Content,
}

impl Order {
    fn key(self, world: &World, entity: Entity) -> Option<u64> {
        match self {
            Self::Entity => Some(entity_key(entity)),
            Self::Key(key) => key(world, entity),
            Self::Content => None,
        }
    }
}

/// The tag of a row's own mixer under [`Order::Content`].
const CONTENT_ROW_TAG: u64 = 0x636f_6e74_656e_7400;

fn component_key<K: DesyncKey>(world: &World, entity: Entity) -> Option<u64> {
    world.get::<K>(entity).map(K::desync_key)
}

/// One folded column: a component over the entities that hold it, or one resource.
trait Column: Send + Sync + 'static {
    fn fold(&mut self, world: &World, order: Order, mix: &mut Mix, scratch: &mut Vec<(u64, Entity)>);
    /// The same column with no cached state (a plugin hands one to each app it builds).
    fn fresh(&self) -> Box<dyn Column>;
    fn type_name(&self) -> &'static str;
}

struct ComponentColumn<C: Component> {
    query: Option<QueryState<Entity, With<C>>>,
    _component: PhantomData<fn() -> C>,
}

impl<C: Component + Serialize> ComponentColumn<C> {
    fn new() -> Self {
        Self {
            query: None,
            _component: PhantomData,
        }
    }
}

impl<C: Component + Serialize> Column for ComponentColumn<C> {
    fn fold(&mut self, world: &World, order: Order, mix: &mut Mix, scratch: &mut Vec<(u64, Entity)>) {
        scratch.clear();
        if self.query.is_none() {
            // `None` until the component is registered; nothing holds it before then.
            self.query = world.try_query_filtered::<Entity, With<C>>();
        }
        let content = matches!(order, Order::Content);
        let mut errors = 0u32;
        if let Some(query) = self.query.as_mut() {
            query.update_archetypes(world);
            for entity in query.iter_manual(world) {
                if content {
                    // The row's own word is its order key (ADR-10062).
                    if let Some(value) = world.get::<C>(entity) {
                        let mut row = Mix::with_rule(CONTENT_ROW_TAG, mix.rule());
                        row.fold(value);
                        errors = errors.saturating_add(row.errors());
                        scratch.push((row.finish(), entity));
                    }
                } else if let Some(key) = order.key(world, entity) {
                    scratch.push((key, entity));
                }
            }
        }
        // Key order (entity id, the game's id, or the row word), never archetype or table order
        // (ADR-8080).
        scratch.sort_unstable();
        if content {
            // A multiset of row words: equal rows are interchangeable, so the entity is not folded.
            for &(word, _) in scratch.iter() {
                mix.u64(word);
            }
            mix.u64(scratch.len() as u64);
            mix.note_errors(errors);
            return;
        }
        let mut rows = 0u64;
        for &(key, entity) in scratch.iter() {
            if let Some(value) = world.get::<C>(entity) {
                mix.u64(key);
                mix.fold(value);
                rows += 1;
            }
        }
        mix.u64(rows);
    }

    fn fresh(&self) -> Box<dyn Column> {
        Box::new(Self::new())
    }

    fn type_name(&self) -> &'static str {
        std::any::type_name::<C>()
    }
}

struct ResourceColumn<R>(PhantomData<fn() -> R>);

impl<R: Resource + Serialize> Column for ResourceColumn<R> {
    fn fold(&mut self, world: &World, _order: Order, mix: &mut Mix, _scratch: &mut Vec<(u64, Entity)>) {
        match world.get_resource::<R>() {
            Some(value) => {
                mix.u64(1);
                mix.fold(value);
            }
            None => mix.u64(0),
        }
    }

    fn fresh(&self) -> Box<dyn Column> {
        Box::new(Self(PhantomData))
    }

    fn type_name(&self) -> &'static str {
        std::any::type_name::<R>()
    }
}

struct Registered {
    system: usize,
    column: Box<dyn Column>,
}

/// A world's per-system desync hash: which components and resources each named system owns, the
/// ring of recorded vectors, and the step counter. A resource ([`DesyncHashPlugin`] inserts it).
///
/// Builder methods panic on a name that is not in the [`Systems`] list (a setup error in the game's
/// code, found the first time it runs).
#[derive(Resource)]
pub struct EcsDesyncHash<const N: usize> {
    systems: Systems<N>,
    columns: Vec<Registered>,
    order: Order,
    tick_source: Option<fn(&World) -> u64>,
    ring: SystemRing<N>,
    last: Option<TraceRow<N>>,
    recorded: u64,
    scratch: Vec<(u64, Entity)>,
}

impl<const N: usize> EcsDesyncHash<N> {
    /// No columns yet; entity-id order; the tick label counts recorded steps from 1.
    #[must_use]
    pub fn new(systems: Systems<N>) -> Self {
        Self {
            systems,
            columns: Vec::new(),
            order: Order::Entity,
            tick_source: None,
            ring: SystemRing::new(),
            last: None,
            recorded: 0,
            scratch: Vec::new(),
        }
    }

    fn index(&self, system: &str) -> usize {
        match self.systems.index_of(system) {
            Some(index) => index,
            None => panic!(
                "desync hash: `{system}` is not one of the systems {:?}",
                self.systems.names()
            ),
        }
    }

    /// Fold component `C` of every entity that holds it into `system`'s word, in entity order.
    /// Columns fold in the order they are added.
    #[must_use]
    pub fn component<C: Component + Serialize>(mut self, system: &str) -> Self {
        let system = self.index(system);
        self.columns.push(Registered {
            system,
            column: Box::new(ComponentColumn::<C>::new()),
        });
        self
    }

    /// Fold resource `R` into `system`'s word (its absence folds too).
    #[must_use]
    pub fn resource<R: Resource + Serialize>(mut self, system: &str) -> Self {
        let system = self.index(system);
        self.columns.push(Registered {
            system,
            column: Box::new(ResourceColumn::<R>(PhantomData)),
        });
        self
    }

    /// Order entities by their `K` component ([`DesyncKey`]) instead of the entity id, and leave out
    /// entities without one.
    #[must_use]
    pub fn order_by<K: DesyncKey>(mut self) -> Self {
        self.order = Order::Key(component_key::<K>);
        self
    }

    /// Order each component column's rows by their own value word instead of the entity id, for a
    /// world whose lockstep entities have no id component (KI-GP-003, ADR-10062). Two peers that
    /// hold the same values under other entity ids (a presentation entity spawned first, or the
    /// same entities spawned in another order) fold the same words; a changed, added, removed or
    /// duplicated value still splits. Entities are not told apart: two entities swapping their
    /// whole values is not seen (give them an id and use [`Self::order_by`] when it matters).
    #[must_use]
    pub fn order_by_content(mut self) -> Self {
        self.order = Order::Content;
        self
    }

    /// Also keep a raw-bits vector per row ([`FloatRule::Raw`], the diagnose ring): each record
    /// folds twice and the ring holds twice the words. Diagnosis only; the canonical words and the
    /// rows peers exchange do not change.
    #[must_use]
    pub fn diagnose_raw(mut self) -> Self {
        self.ring = SystemRing::with_raw();
        self
    }

    /// Label each recorded row with `source(world)` (the game's lockstep step label, the same on
    /// every peer) instead of the count of recorded steps.
    #[must_use]
    pub fn tick_from(mut self, source: fn(&World) -> u64) -> Self {
        self.tick_source = Some(source);
        self
    }

    /// The system names.
    pub fn systems(&self) -> &Systems<N> {
        &self.systems
    }

    /// `(system name, type name)` of every column, in fold order.
    pub fn columns(&self) -> Vec<(&'static str, &'static str)> {
        self.columns
            .iter()
            .map(|c| (self.systems.names()[c.system], c.column.type_name()))
            .collect()
    }

    /// Fold every system of `world` now. No allocation once the scratch buffer has grown to the
    /// largest column.
    pub fn hashes(&mut self, world: &World) -> SystemHashes<N> {
        self.fold(world, FloatRule::Canonical).hashes
    }

    /// Fold every system of `world` under float rule `rule`, with the partial flag (a failing
    /// `Serialize`).
    pub fn fold(&mut self, world: &World, rule: FloatRule) -> Folded<N> {
        // The same tags as `regia_detsim::hash::system_hashes`: system `i` mixes under `i + 1`.
        let mut mix: [Mix; N] = std::array::from_fn(|i| Mix::with_rule(i as u64 + 1, rule));
        for Registered { system, column } in &mut self.columns {
            column.fold(world, self.order, &mut mix[*system], &mut self.scratch);
        }
        finish_mixers(mix)
    }

    /// Fold `world` and keep the row under `tick` (a rewind replaces the rows from `tick` on). With
    /// [`EcsDesyncHash::diagnose_raw`] the raw-bits vector is folded and kept too; a partial fold is
    /// flagged on the row.
    pub fn record(&mut self, world: &World, tick: u64) -> TraceRow<N> {
        let folded = self.fold(world, FloatRule::Canonical);
        let raw = self
            .ring
            .raw_enabled()
            .then(|| self.fold(world, FloatRule::Raw).hashes);
        let row = TraceRow {
            tick,
            hashes: folded.hashes,
        };
        self.ring.push_row(RingRow {
            tick,
            hashes: folded.hashes,
            raw,
            partial: folded.partial,
        });
        self.last = Some(row);
        self.recorded += 1;
        row
    }

    /// The recorded rows (the last [`RING_TICKS`]).
    pub fn ring(&self) -> &SystemRing<N> {
        &self.ring
    }

    /// The newest recorded row.
    pub fn last(&self) -> Option<TraceRow<N>> {
        self.last
    }

    /// Steps recorded since the hash was created.
    pub fn recorded(&self) -> u64 {
        self.recorded
    }

    fn next_tick(&self, world: &World) -> u64 {
        match self.tick_source {
            Some(source) => source(world),
            None => self.recorded + 1,
        }
    }

    fn fresh(&self) -> Self {
        Self {
            systems: self.systems,
            columns: self
                .columns
                .iter()
                .map(|c| Registered {
                    system: c.system,
                    column: c.column.fresh(),
                })
                .collect(),
            order: self.order,
            tick_source: self.tick_source,
            ring: if self.ring.raw_enabled() {
                SystemRing::with_raw()
            } else {
                SystemRing::new()
            },
            last: None,
            recorded: 0,
            scratch: Vec::new(),
        }
    }

    /// Insert this hash into `world` with the type-erased access [`desync_rows`] and
    /// [`compare_desync_rows`] read (one per world: a second replaces the first).
    pub fn insert_into(self, world: &mut World) {
        world.insert_resource(DesyncRowsAccess {
            rows: rows_of::<N>,
            compare: compare_of::<N>,
        });
        world.insert_resource(self);
    }
}

/// Fold and record one row of the world's [`EcsDesyncHash<N>`] (no-op without one). The system
/// [`DesyncHashPlugin`] adds; a host that steps its lockstep schedule by hand may call it after each
/// step instead.
pub fn record_desync_hashes<const N: usize>(world: &mut World) {
    let _span = tracing::trace_span!("desync_hash_record").entered();
    if !world.contains_resource::<EcsDesyncHash<N>>() {
        return;
    }
    world.resource_scope(|world, mut hash: Mut<EcsDesyncHash<N>>| {
        let tick = hash.next_tick(world);
        hash.record(world, tick);
    });
}

/// Installs an [`EcsDesyncHash<N>`] and records a row after every step.
///
/// The builder methods mirror [`EcsDesyncHash`]'s. The record system runs in `FixedUpdate` by
/// default, in [`DesyncHashSet`], after every set given to [`DesyncHashPlugin::after`].
pub struct DesyncHashPlugin<const N: usize> {
    hash: EcsDesyncHash<N>,
    schedule: InternedScheduleLabel,
    after: Vec<InternedSystemSet>,
}

impl<const N: usize> DesyncHashPlugin<N> {
    /// A plugin for `systems` with no columns, recording in `FixedUpdate`.
    #[must_use]
    pub fn new(systems: Systems<N>) -> Self {
        Self {
            hash: EcsDesyncHash::new(systems),
            schedule: FixedUpdate.intern(),
            after: Vec::new(),
        }
    }

    /// See [`EcsDesyncHash::component`].
    #[must_use]
    pub fn component<C: Component + Serialize>(mut self, system: &str) -> Self {
        self.hash = self.hash.component::<C>(system);
        self
    }

    /// See [`EcsDesyncHash::resource`].
    #[must_use]
    pub fn resource<R: Resource + Serialize>(mut self, system: &str) -> Self {
        self.hash = self.hash.resource::<R>(system);
        self
    }

    /// See [`EcsDesyncHash::order_by_content`].
    #[must_use]
    pub fn order_by_content(mut self) -> Self {
        self.hash = self.hash.order_by_content();
        self
    }

    /// See [`EcsDesyncHash::order_by`].
    #[must_use]
    pub fn order_by<K: DesyncKey>(mut self) -> Self {
        self.hash = self.hash.order_by::<K>();
        self
    }

    /// See [`EcsDesyncHash::diagnose_raw`].
    #[must_use]
    pub fn diagnose_raw(mut self) -> Self {
        self.hash = self.hash.diagnose_raw();
        self
    }

    /// See [`EcsDesyncHash::tick_from`].
    #[must_use]
    pub fn tick_from(mut self, source: fn(&World) -> u64) -> Self {
        self.hash = self.hash.tick_from(source);
        self
    }

    /// Record in `schedule` instead of `FixedUpdate`.
    #[must_use]
    pub fn in_schedule(mut self, schedule: impl ScheduleLabel) -> Self {
        self.schedule = schedule.intern();
        self
    }

    /// Record after `set` (one of the game's lockstep schedule sets, or a system) in the record
    /// schedule.
    #[must_use]
    pub fn after<M>(mut self, set: impl IntoSystemSet<M>) -> Self {
        self.after.push(set.into_system_set().intern());
        self
    }
}

impl<const N: usize> Plugin for DesyncHashPlugin<N> {
    fn build(&self, app: &mut App) {
        for (i, name) in self.hash.systems.names().iter().enumerate() {
            if !self.hash.columns.iter().any(|c| c.system == i) {
                tracing::warn!(
                    target: "regia_framework::desync_hash",
                    "desync hash: system `{name}` has no component or resource; its word never changes"
                );
            }
        }
        if app.world().contains_resource::<DesyncRowsAccess>() {
            tracing::warn!(
                target: "regia_framework::desync_hash",
                "desync hash: a second DesyncHashPlugin replaces the first (one per app)"
            );
        }
        self.hash.fresh().insert_into(app.world_mut());
        app.configure_sets(self.schedule, DesyncHashSet);
        for set in &self.after {
            app.configure_sets(self.schedule, DesyncHashSet.after(*set));
        }
        app.add_systems(self.schedule, record_desync_hashes::<N>.in_set(DesyncHashSet));
    }
}

/// One recorded row with its words in a `Vec` (the type-erased form of [`RingRow`]).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DesyncRow {
    /// The step label.
    pub tick: u64,
    /// One word per system, in [`Systems`] order (the canonical rule).
    pub words: Vec<u64>,
    /// The raw-bits words ([`FloatRule::Raw`]) on a diagnose ring; `None` otherwise.
    pub raw: Option<Vec<u64>>,
    /// A `Serialize` failed while folding this row: some words cover part of the state.
    pub partial: bool,
}

impl DesyncRow {
    /// A whole canonical row (no raw words).
    pub fn new(tick: u64, words: Vec<u64>) -> Self {
        Self {
            tick,
            words,
            raw: None,
            partial: false,
        }
    }

    fn from_ring<const N: usize>(row: &RingRow<N>) -> Self {
        Self {
            tick: row.tick,
            words: row.hashes.words.to_vec(),
            raw: row.raw.map(|raw| raw.words.to_vec()),
            partial: row.partial,
        }
    }
}

/// Where [`desync_rows`] read the rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesyncSource {
    /// The world's [`EcsDesyncHash`] ([`DesyncHashPlugin`]).
    Ecs,
    /// The [`SimHostHandle`](crate::sim_host::SimHostHandle)'s external sim ([`DesyncFeed`]).
    SimHost,
}

impl DesyncSource {
    /// `ecs` or `sim_host` (the agent protocol's `source`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ecs => "ecs",
            Self::SimHost => "sim_host",
        }
    }
}

/// What [`desync_rows`] returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesyncRows {
    /// The system names, in word order.
    pub systems: Vec<&'static str>,
    /// The newest recorded step label.
    pub newest: Option<u64>,
    /// Steps recorded since the hash was created.
    pub recorded: u64,
    /// The asked window, oldest first.
    pub rows: Vec<DesyncRow>,
    /// Where the rows come from.
    pub source: DesyncSource,
    /// The ring keeps raw-bits words ([`EcsDesyncHash::diagnose_raw`], [`DesyncFeed::new`]).
    pub raw: bool,
    /// Rows recorded with a partial fold since the ring was made (they may have scrolled out).
    pub partial_rows: u64,
}

/// A row given to [`compare_desync_rows`] holds the wrong number of words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WordCountMismatch {
    /// The row's tick.
    pub tick: u64,
    /// Words the row holds.
    pub words: usize,
    /// Systems this world hashes.
    pub systems: usize,
}

/// What comparing two row sets gives: `None` without an [`EcsDesyncHash<N>`] in the world.
type CompareOutcome = Option<Result<Diagnosis, WordCountMismatch>>;

/// Monomorphized readers of the world's [`EcsDesyncHash<N>`], so callers that do not know `N` (the
/// agent protocol) can read it.
#[derive(Resource, Clone, Copy)]
struct DesyncRowsAccess {
    rows: fn(&World, Option<u64>, usize) -> Option<DesyncRows>,
    compare: fn(&World, &[DesyncRow], &[DesyncRow]) -> CompareOutcome,
}

fn window_of<const N: usize>(
    systems: &Systems<N>,
    ring: &SystemRing<N>,
    newest: Option<u64>,
    recorded: u64,
    to: Option<u64>,
    count: usize,
    source: DesyncSource,
) -> DesyncRows {
    let rows = match to.or(newest) {
        Some(to) => ring
            .window_rows(to, count)
            .iter()
            .map(DesyncRow::from_ring)
            .collect(),
        None => Vec::new(),
    };
    DesyncRows {
        systems: systems.names().to_vec(),
        newest,
        recorded,
        rows,
        source,
        raw: ring.raw_enabled(),
        partial_rows: ring.partial_rows(),
    }
}

fn rows_of<const N: usize>(world: &World, to: Option<u64>, count: usize) -> Option<DesyncRows> {
    let hash = world.get_resource::<EcsDesyncHash<N>>()?;
    Some(window_of(
        &hash.systems,
        &hash.ring,
        hash.last.map(|row| row.tick),
        hash.recorded,
        to,
        count,
        DesyncSource::Ecs,
    ))
}

fn words_of<const N: usize>(tick: u64, words: &[u64]) -> Result<SystemHashes<N>, WordCountMismatch> {
    let words: [u64; N] = words.try_into().map_err(|_| WordCountMismatch {
        tick,
        words: words.len(),
        systems: N,
    })?;
    Ok(SystemHashes { words })
}

fn typed_rows<const N: usize>(rows: &[DesyncRow]) -> Result<Vec<RingRow<N>>, WordCountMismatch> {
    rows.iter()
        .map(|row| {
            Ok(RingRow {
                tick: row.tick,
                hashes: words_of::<N>(row.tick, &row.words)?,
                raw: match &row.raw {
                    Some(raw) => Some(words_of::<N>(row.tick, raw)?),
                    None => None,
                },
                partial: row.partial,
            })
        })
        .collect()
}

fn compare_of<const N: usize>(world: &World, ours: &[DesyncRow], theirs: &[DesyncRow]) -> CompareOutcome {
    let hash = world.get_resource::<EcsDesyncHash<N>>()?;
    Some(compare_typed(&hash.systems, ours, theirs))
}

fn compare_typed<const N: usize>(
    systems: &Systems<N>,
    ours: &[DesyncRow],
    theirs: &[DesyncRow],
) -> Result<Diagnosis, WordCountMismatch> {
    let ours = typed_rows::<N>(ours)?;
    let theirs = typed_rows::<N>(theirs)?;
    Ok(diagnose(systems, &ours, &theirs))
}

fn host_desync(world: &World) -> Option<&SimHostDesync> {
    world.get_resource::<crate::sim_host::SimHostHandle>()?.desync()
}

/// The recorded rows with `to - count < tick <= to` (`to` defaults to the newest row; `count` is
/// capped at [`MAX_TRACE_ROWS`]): the world's [`EcsDesyncHash`], or, without one, the external sim
/// behind the world's [`SimHostHandle`](crate::sim_host::SimHostHandle) when it records a
/// [`DesyncFeed`]. `None` when neither records.
pub fn desync_rows(world: &World, to: Option<u64>, count: usize) -> Option<DesyncRows> {
    match world.get_resource::<DesyncRowsAccess>() {
        Some(access) => (access.rows)(world, to, count),
        None => host_desync(world).map(|desync| desync.rows(to, count)),
    }
}

/// Join `ours` and `theirs` on tick and name each system that split and the first tick it did
/// ([`diverged_systems`] with the names of the rows [`desync_rows`] reads). `None` when nothing
/// records.
pub fn compare_desync_rows(
    world: &World,
    ours: &[DesyncRow],
    theirs: &[DesyncRow],
) -> Option<Result<DivergenceReport, WordCountMismatch>> {
    diagnose_desync_rows(world, ours, theirs).map(|outcome| outcome.map(|d| d.canonical))
}

/// [`compare_desync_rows`] with the raw-bits report, the zero-sign or NaN-payload splits
/// ([`Diagnosis::float_bits`]) and the first partial row ([`diagnose`]).
pub fn diagnose_desync_rows(
    world: &World,
    ours: &[DesyncRow],
    theirs: &[DesyncRow],
) -> Option<Result<Diagnosis, WordCountMismatch>> {
    match world.get_resource::<DesyncRowsAccess>() {
        Some(access) => (access.compare)(world, ours, theirs),
        None => host_desync(world).map(|desync| desync.diagnose(ours, theirs)),
    }
}

// ---------------------------------------------------------------------------------------------------
// External sims (ADR-8547)
// ---------------------------------------------------------------------------------------------------

/// The typed ring behind a [`DesyncFeed`]: the systems, the ring and the counters.
struct FeedRing<const N: usize> {
    ring: SystemRing<N>,
    newest: Option<u64>,
    recorded: u64,
}

struct FeedCell<const N: usize> {
    systems: Systems<N>,
    raw: bool,
    state: Mutex<FeedRing<N>>,
}

impl<const N: usize> FeedCell<N> {
    fn lock(&self) -> std::sync::MutexGuard<'_, FeedRing<N>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Type-erased reads of a [`DesyncFeed`]'s ring.
trait DesyncReader: Send + Sync {
    fn rows(&self, to: Option<u64>, count: usize) -> DesyncRows;
    fn diagnose(&self, ours: &[DesyncRow], theirs: &[DesyncRow]) -> Result<Diagnosis, WordCountMismatch>;
}

impl<const N: usize> DesyncReader for FeedCell<N> {
    fn rows(&self, to: Option<u64>, count: usize) -> DesyncRows {
        let state = self.lock();
        window_of(
            &self.systems,
            &state.ring,
            state.newest,
            state.recorded,
            to,
            count,
            DesyncSource::SimHost,
        )
    }

    fn diagnose(&self, ours: &[DesyncRow], theirs: &[DesyncRow]) -> Result<Diagnosis, WordCountMismatch> {
        compare_typed(&self.systems, ours, theirs)
    }
}

/// The recording side of an external sim's desync ring: the sim's thread folds its
/// [`SystemFold`] after every step and keeps the row; readers on other threads hold the
/// [`SimHostDesync`] from [`DesyncFeed::reader`].
///
/// The fold runs outside the lock; the lock covers one row push (no allocation once the ring is
/// full). The threaded sim host makes one for a sim that implements `SystemFold`
/// ([`ThreadedSimHost::start_with_desync`](crate::sim_host::ThreadedSimHost::start_with_desync)).
pub struct DesyncFeed<const N: usize> {
    cell: Arc<FeedCell<N>>,
}

impl<const N: usize> Clone for DesyncFeed<N> {
    fn clone(&self) -> Self {
        Self {
            cell: Arc::clone(&self.cell),
        }
    }
}

impl<const N: usize> std::fmt::Debug for DesyncFeed<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesyncFeed")
            .field("systems", self.cell.systems.names())
            .field("raw", &self.cell.raw)
            .finish_non_exhaustive()
    }
}

impl<const N: usize> DesyncFeed<N> {
    /// An empty ring for `systems`; `raw` also keeps the raw-bits vector per row (the diagnose
    /// ring, [`SystemRing::with_raw`]).
    pub fn new(systems: Systems<N>, raw: bool) -> Self {
        Self {
            cell: Arc::new(FeedCell {
                systems,
                raw,
                state: Mutex::new(FeedRing {
                    ring: if raw {
                        SystemRing::with_raw()
                    } else {
                        SystemRing::new()
                    },
                    newest: None,
                    recorded: 0,
                }),
            }),
        }
    }

    /// Fold `state` and keep the row under `tick` (a rewind replaces the rows from `tick` on).
    pub fn record<S: SystemFold<N> + ?Sized>(&self, tick: u64, state: &S) {
        self.keep(tick, state, false);
    }

    /// Drop every row, then fold `state` and keep it under `tick`, in one step under the lock (a
    /// reader never sees the empty ring). A restore to `tick` starts a new timeline: rows recorded
    /// before it, older ticks included, belong to the old one (ADR-8621). `recorded` keeps counting.
    pub fn restart<S: SystemFold<N> + ?Sized>(&self, tick: u64, state: &S) {
        self.keep(tick, state, true);
    }

    fn keep<S: SystemFold<N> + ?Sized>(&self, tick: u64, state: &S, clear: bool) {
        let _span = tracing::trace_span!("desync_feed_record").entered();
        let folded = fold_system_hashes(state, FloatRule::Canonical);
        let raw = self
            .cell
            .raw
            .then(|| fold_system_hashes(state, FloatRule::Raw).hashes);
        let mut ring = self.cell.lock();
        if clear {
            ring.ring.clear();
        }
        ring.ring.push_row(RingRow {
            tick,
            hashes: folded.hashes,
            raw,
            partial: folded.partial,
        });
        ring.newest = Some(tick);
        ring.recorded += 1;
    }

    /// The rows with `to - count < tick <= to` (typed).
    pub fn window_rows(&self, to: u64, count: usize) -> Vec<RingRow<N>> {
        self.cell.lock().ring.window_rows(to, count)
    }

    /// A type-erased reader of this ring (what the host handle carries).
    pub fn reader(&self) -> SimHostDesync {
        SimHostDesync {
            reader: Arc::clone(&self.cell) as Arc<dyn DesyncReader>,
        }
    }
}

/// A type-erased reader of an external sim's desync ring ([`DesyncFeed::reader`]); the
/// [`SimHostHandle`](crate::sim_host::SimHostHandle) of a host started with
/// [`ThreadedSimHost::start_with_desync`](crate::sim_host::ThreadedSimHost::start_with_desync)
/// carries one, and [`desync_rows`] reads it when the world has no [`EcsDesyncHash`].
#[derive(Clone)]
pub struct SimHostDesync {
    reader: Arc<dyn DesyncReader>,
}

impl std::fmt::Debug for SimHostDesync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimHostDesync").finish_non_exhaustive()
    }
}

impl SimHostDesync {
    /// The rows with `to - count < tick <= to` (`to` defaults to the newest; `count` capped at
    /// [`MAX_TRACE_ROWS`]).
    pub fn rows(&self, to: Option<u64>, count: usize) -> DesyncRows {
        self.reader.rows(to, count)
    }

    /// [`diagnose`] `ours` against `theirs` with this ring's system names.
    pub fn diagnose(&self, ours: &[DesyncRow], theirs: &[DesyncRow]) -> Result<Diagnosis, WordCountMismatch> {
        self.reader.diagnose(ours, theirs)
    }
}

#[cfg(test)]
mod tests;
