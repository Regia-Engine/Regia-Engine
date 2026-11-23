// SPDX-License-Identifier: MPL-2.0
//! The [`Choosers`] resource (tables by asset path, startup load, hot reload that applies only
//! valid files), per-entity [`ChooserBinding`]s evaluated in [`ChooserSet::Evaluate`]
//! (`FixedUpdate`, once per fixed step), the [`ChooserPicked`] message and the
//! [`ChooserTraceLog`] ring.

use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::{AssetServer, SourceAssetChanged};
use regia_core::math::Rng;
use regia_scene::components::EntityGuid;

use super::{
    CHOOSER_TYPE_PATH, ChooserContext, ChooserEnv, ChooserIssue, ChooserOutcome, ChooserTable, ChooserTrace,
    RecentPicks, ResultEntry, TraceDetail,
};
use crate::table_source::{TableBytes, read_table};

/// One loaded table and its bookkeeping.
#[derive(Debug, Clone)]
pub struct ChooserSlot {
    /// The table in use (an empty `OutputOnly` table when the first load failed).
    pub table: ChooserTable,
    /// Bumped on every applied reload or edit (1 after the first load).
    pub generation: u64,
    /// Issues of the last load, reload, edit or validation (a refused reload's errors stay here).
    pub last_issues: Vec<ChooserIssue>,
    /// The source file, when the app has an asset server.
    pub file: Option<PathBuf>,
    /// Applied reloads.
    pub reloads: u32,
    /// Reloads refused because the new text did not load.
    pub rejected_reloads: u32,
    /// Whether [`Self::table`] came from a cooked payload rather than source text.
    pub cooked: bool,
}

impl Default for ChooserSlot {
    fn default() -> Self {
        Self {
            table: ChooserTable::new(super::Mode::First, super::ResultType::OutputOnly),
            generation: 0,
            last_issues: Vec::new(),
            file: None,
            reloads: 0,
            rejected_reloads: 0,
            cooked: false,
        }
    }
}

/// Every chooser the app loaded, by asset path (`anim/hit.chooser.ron`); resolves nested
/// `Chooser(..)` results.
#[derive(Resource, Debug, Default)]
pub struct Choosers {
    slots: BTreeMap<String, ChooserSlot>,
}

impl ChooserEnv for Choosers {
    fn chooser(&self, path: &str) -> Option<&ChooserTable> {
        self.get(path)
    }
}

impl Choosers {
    /// The table at `path`.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&ChooserTable> {
        self.slots.get(path).map(|slot| &slot.table)
    }

    /// The table's generation (0 when there is no table at `path`).
    #[must_use]
    pub fn generation(&self, path: &str) -> u64 {
        self.slots.get(path).map_or(0, |slot| slot.generation)
    }

    /// The slot at `path`.
    #[must_use]
    pub fn slot(&self, path: &str) -> Option<&ChooserSlot> {
        self.slots.get(path)
    }

    /// Every slot, sorted by path.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ChooserSlot)> {
        self.slots.iter().map(|(path, slot)| (path.as_str(), slot))
    }

    /// The number of tables.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether there are no tables.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Puts `table` at `path` (replacing any table there), re-validates it against the other
    /// tables, and returns its new generation.
    pub fn insert(&mut self, path: &str, table: ChooserTable) -> u64 {
        let slot = self.slots.entry(path.to_owned()).or_default();
        slot.table = table;
        slot.generation += 1;
        let generation = slot.generation;
        self.revalidate(path);
        generation
    }

    /// Re-runs [`ChooserTable::validate_at`] for `path` and stores the issues.
    pub fn revalidate(&mut self, path: &str) {
        let Some(slot) = self.slots.get(path) else {
            return;
        };
        let issues = slot.table.validate_at(Some(path), self);
        if let Some(slot) = self.slots.get_mut(path) {
            slot.last_issues = issues;
        }
    }

    /// Removes the table at `path`.
    pub fn remove(&mut self, path: &str) -> Option<ChooserSlot> {
        self.slots.remove(path)
    }

    /// Loads `text` into `path`. A text that does not load leaves the previous table in use and
    /// keeps the errors in [`ChooserSlot::last_issues`].
    ///
    /// # Errors
    ///
    /// The issues of a text that did not load.
    pub fn load_text(&mut self, path: &str, text: &str) -> Result<u64, Vec<ChooserIssue>> {
        self.apply_load(path, ChooserTable::from_ron_str(text), false)
    }

    /// Loads a cooked payload ([`ChooserTable::to_cooked`]) into `path`, with the same rules as
    /// [`load_text`](Self::load_text).
    ///
    /// # Errors
    ///
    /// The issues of a payload that did not load.
    pub fn load_cooked(&mut self, path: &str, payload: &[u8]) -> Result<u64, Vec<ChooserIssue>> {
        self.apply_load(path, ChooserTable::from_cooked(payload), true)
    }

    fn apply_load(
        &mut self,
        path: &str,
        loaded: Result<ChooserTable, super::ChooserError>,
        cooked: bool,
    ) -> Result<u64, Vec<ChooserIssue>> {
        match loaded {
            Ok(table) => {
                let generation = self.insert(path, table);
                if let Some(slot) = self.slots.get_mut(path) {
                    slot.cooked = cooked;
                    if generation > 1 {
                        slot.reloads += 1;
                    }
                }
                Ok(generation)
            }
            Err(error) => {
                let slot = self.slots.entry(path.to_owned()).or_default();
                if slot.generation > 0 {
                    slot.rejected_reloads += 1;
                }
                slot.last_issues.clone_from(&error.issues);
                Err(error.issues)
            }
        }
    }

    /// Applies `edit` to a copy of the table at `path` and keeps it only when the result has no
    /// structural error; on success bumps the generation and returns it.
    ///
    /// # Errors
    ///
    /// `chooser_not_found`, the edit's own issue, or the structural errors (the table is unchanged).
    pub fn edit(
        &mut self,
        path: &str,
        edit: impl FnOnce(&mut ChooserTable) -> Result<(), ChooserIssue>,
    ) -> Result<u64, Vec<ChooserIssue>> {
        let Some(slot) = self.slots.get(path) else {
            return Err(vec![ChooserIssue::error(
                "chooser_not_found",
                format!("no chooser at `{path}`"),
            )]);
        };
        let mut table = slot.table.clone();
        edit(&mut table).map_err(|issue| vec![issue])?;
        let issues = table.validate_structure();
        if issues.iter().any(ChooserIssue::is_error) {
            return Err(issues);
        }
        Ok(self.insert(path, table))
    }
}

/// When a binding re-evaluates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Reeval {
    /// When the context the provider fills differs from the last one (floats compared by bits),
    /// or the table or its generation changed.
    #[default]
    OnChange,
    /// Every fixed step.
    EveryTick,
    /// Only after [`ChooserBinding::request`].
    Manual,
}

/// Re-evaluates a chooser for one entity with a context its provider fills (spec 51 § 2.7).
///
/// Reflected (opaque, serde) so a demo snapshot carries the binding, the random stream included:
/// a seek restores the stream and the picks continue where the recording left off (audit G
/// finding 4, ADR-4756). The per-evaluation cache (`last_pick`, `last_context`, `last_table`,
/// `requested`) is serialized too (Muse read-only review of fix-framework2-determinism, high
/// finding 2): a restored `Reeval::OnChange` binding with an unchanged context must skip without
/// drawing (a fresh draw would consume a random number the straight run never took and diverge
/// the stream from the recording) and must not re-raise an unchanged pick.
#[derive(Component, Debug, Clone, Reflect, serde::Serialize, serde::Deserialize)]
#[reflect(Component, Serialize, Deserialize, opaque)]
pub struct ChooserBinding {
    /// Asset path of the table; swap it to switch a whole set (the next evaluation uses the new one).
    pub table: String,
    /// Name of the provider registered with [`ChooserAppExt::register_chooser_provider`].
    pub provider: String,
    /// Evaluation site label (trace log key; agent `chooser.trace { site }`).
    pub site: String,
    /// When to re-evaluate.
    pub reeval: Reeval,
    /// Seed of the binding's own random stream. The stream id is the entity's [`EntityGuid`]
    /// (mixed to 64 bits) when it has one, else its entity bits (spawn-order dependent; games
    /// give bound entities a guid for determinism, audit G finding 4, ADR-4756).
    pub seed: u64,
    /// The binding's random stream, created at the first evaluation (saved with the binding).
    pub rng: Option<Rng>,
    /// Recent `Weighted` picks of this binding.
    pub recent: RecentPicks,
    /// The last pick (saved with the binding, so a restore compares against it instead of
    /// re-raising an unchanged pick).
    #[serde(
        serialize_with = "binding_cache::serialize_outcome",
        deserialize_with = "binding_cache::deserialize_outcome",
        default
    )]
    pub last_pick: Option<ChooserOutcome>,
    /// Evaluations so far.
    pub evaluations: u64,
    /// The last context (saved with the binding; the `OnChange` comparison survives a restore).
    #[serde(
        serialize_with = "binding_cache::serialize_context",
        deserialize_with = "binding_cache::deserialize_context",
        default
    )]
    last_context: Option<ChooserContext>,
    #[serde(default)]
    last_table: Option<(String, u64)>,
    #[serde(default)]
    requested: bool,
}

impl ChooserBinding {
    /// A binding of `table` filled by `provider`, evaluated on change.
    #[must_use]
    pub fn new(table: &str, provider: &str, site: &str) -> Self {
        Self {
            table: table.to_owned(),
            provider: provider.to_owned(),
            site: site.to_owned(),
            reeval: Reeval::OnChange,
            seed: 0,
            rng: None,
            recent: RecentPicks::default(),
            last_pick: None,
            evaluations: 0,
            last_context: None,
            last_table: None,
            requested: false,
        }
    }

    /// Sets when to re-evaluate.
    #[must_use]
    pub fn with_reeval(mut self, reeval: Reeval) -> Self {
        self.reeval = reeval;
        self
    }

    /// Sets the seed.
    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Asks a [`Reeval::Manual`] binding (or any binding) to evaluate on the next step.
    pub fn request(&mut self) {
        self.requested = true;
    }

    /// The id of this binding's random stream when it is evaluated for `entity` in `world`: the
    /// entity's [`EntityGuid`] (mixed to 64 bits) when it has one, else its entity bits. This is
    /// the same key [`evaluate_bindings`] uses, so an out-of-band evaluation (the agent's
    /// `chooser.evaluate_binding`) never seeds a never-yet-evaluated binding on the
    /// spawn-order-dependent fallback stream (Muse read-only review of
    /// fix-framework2-determinism, medium finding 4).
    #[must_use]
    pub fn stream_id(world: &World, entity: Entity) -> u64 {
        binding_stream(world, entity)
    }
}

/// The serde shadow of [`ChooserBinding`]'s per-evaluation cache. Keeps the public chooser types
/// free of serde impls: the cache converts to/from these private mirrors (Muse read-only review
/// of fix-framework2-determinism, high finding 2).
mod binding_cache {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};

    use super::super::{ChooserValue, ResultEntry};
    use super::{ChooserContext, ChooserOutcome};

    #[derive(Serialize, Deserialize)]
    enum SavedValue {
        Bool(bool),
        Int(i64),
        Float(f32),
        Enum(String),
        Name(String),
        Tags(Vec<String>),
        Asset(String),
    }

    impl From<&ChooserValue> for SavedValue {
        fn from(value: &ChooserValue) -> Self {
            match value {
                ChooserValue::Bool(b) => Self::Bool(*b),
                ChooserValue::Int(i) => Self::Int(*i),
                ChooserValue::Float(f) => Self::Float(*f),
                ChooserValue::Enum(s) => Self::Enum(s.clone()),
                ChooserValue::Name(s) => Self::Name(s.clone()),
                ChooserValue::Tags(tags) => Self::Tags(tags.names().into_iter().map(str::to_owned).collect()),
                ChooserValue::Asset(s) => Self::Asset(s.clone()),
            }
        }
    }

    impl From<SavedValue> for ChooserValue {
        fn from(saved: SavedValue) -> Self {
            match saved {
                SavedValue::Bool(b) => Self::Bool(b),
                SavedValue::Int(i) => Self::Int(i),
                SavedValue::Float(f) => Self::Float(f),
                SavedValue::Enum(s) => Self::Enum(s),
                SavedValue::Name(s) => Self::Name(s),
                // Tags valid in a saved container stay valid; a corrupt entry drops instead of
                // failing the whole restore.
                SavedValue::Tags(names) => Self::Tags(
                    names
                        .iter()
                        .filter_map(|n| crate::tags::Tag::new(n).ok())
                        .collect(),
                ),
                SavedValue::Asset(s) => Self::Asset(s),
            }
        }
    }

    #[derive(Serialize, Deserialize)]
    struct SavedContext {
        inputs: BTreeMap<String, SavedValue>,
        outputs: BTreeMap<String, SavedValue>,
    }

    impl From<&ChooserContext> for SavedContext {
        fn from(ctx: &ChooserContext) -> Self {
            Self {
                inputs: ctx.inputs().map(|(k, v)| (k.to_owned(), v.into())).collect(),
                outputs: ctx.outputs().map(|(k, v)| (k.to_owned(), v.into())).collect(),
            }
        }
    }

    impl From<SavedContext> for ChooserContext {
        fn from(saved: SavedContext) -> Self {
            let mut ctx = Self::default();
            for (name, value) in saved.inputs {
                ctx.set(&name, value.into());
            }
            for (name, value) in saved.outputs {
                ctx.write_output(&name, value.into());
            }
            ctx
        }
    }

    /// A stored outcome's result is always a leaf: evaluation resolves nested `Chooser`/`Inline`
    /// entries to what the nested table selected (eval.rs `resolve`). `Inline` is mapped
    /// defensively to `Nothing` on that unreachable path.
    #[derive(Serialize, Deserialize)]
    enum SavedResult {
        Asset(String),
        Value(SavedValue),
        Chooser(String),
        Nothing,
    }

    impl From<&ResultEntry> for SavedResult {
        fn from(entry: &ResultEntry) -> Self {
            match entry {
                ResultEntry::Asset(s) => Self::Asset(s.clone()),
                ResultEntry::Value(v) => Self::Value(v.into()),
                ResultEntry::Chooser(s) => Self::Chooser(s.clone()),
                ResultEntry::Inline(_) | ResultEntry::Nothing => Self::Nothing,
            }
        }
    }

    impl From<SavedResult> for ResultEntry {
        fn from(saved: SavedResult) -> Self {
            match saved {
                SavedResult::Asset(s) => Self::Asset(s),
                SavedResult::Value(v) => Self::Value(v.into()),
                SavedResult::Chooser(s) => Self::Chooser(s),
                SavedResult::Nothing => Self::Nothing,
            }
        }
    }

    #[derive(Serialize, Deserialize)]
    struct SavedOutcome {
        result: SavedResult,
        row: Option<String>,
        used_catch_all: bool,
        outputs: Vec<(String, SavedValue)>,
    }

    impl From<&ChooserOutcome> for SavedOutcome {
        fn from(outcome: &ChooserOutcome) -> Self {
            Self {
                result: (&outcome.result).into(),
                row: outcome.row.clone(),
                used_catch_all: outcome.used_catch_all,
                outputs: outcome
                    .outputs
                    .iter()
                    .map(|(k, v)| (k.clone(), v.into()))
                    .collect(),
            }
        }
    }

    impl From<SavedOutcome> for ChooserOutcome {
        fn from(saved: SavedOutcome) -> Self {
            Self {
                result: saved.result.into(),
                row: saved.row,
                used_catch_all: saved.used_catch_all,
                outputs: saved.outputs.into_iter().map(|(k, v)| (k, v.into())).collect(),
            }
        }
    }

    pub(super) fn serialize_context<S>(
        value: &Option<ChooserContext>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match value {
            None => serializer.serialize_none(),
            Some(ctx) => serializer.serialize_some(&SavedContext::from(ctx)),
        }
    }

    pub(super) fn deserialize_context<'de, D>(deserializer: D) -> Result<Option<ChooserContext>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Option::<SavedContext>::deserialize(deserializer).map(|saved| saved.map(ChooserContext::from))
    }

    pub(super) fn serialize_outcome<S>(
        value: &Option<ChooserOutcome>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match value {
            None => serializer.serialize_none(),
            Some(outcome) => serializer.serialize_some(&SavedOutcome::from(outcome)),
        }
    }

    pub(super) fn deserialize_outcome<'de, D>(deserializer: D) -> Result<Option<ChooserOutcome>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Option::<SavedOutcome>::deserialize(deserializer).map(|saved| saved.map(ChooserOutcome::from))
    }
}

/// Raised (as a message) when a binding's pick differs from its previous one.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct ChooserPicked {
    /// The bound entity.
    pub entity: Entity,
    /// The binding's site.
    pub site: String,
    /// The table's asset path.
    pub table: String,
    /// The selected row (`None` for the catch-all).
    pub row: Option<String>,
    /// The result.
    pub result: ResultEntry,
    /// Whether the catch-all answered.
    pub used_catch_all: bool,
    /// The outputs the pick wrote.
    pub outputs: Vec<(String, super::ChooserValue)>,
}

/// Fills a binding's context from the world (game code; the one place inputs come from components).
pub type ProviderFn = fn(&World, Entity, &mut ChooserContext);

/// Registered context providers by name.
#[derive(Resource, Debug, Default, Clone)]
pub struct ChooserProviders {
    providers: BTreeMap<String, ProviderFn>,
}

impl ChooserProviders {
    /// Registers (or replaces) provider `name`.
    pub fn register(&mut self, name: &str, provider: ProviderFn) {
        self.providers.insert(name.to_owned(), provider);
    }

    /// Provider `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<ProviderFn> {
        self.providers.get(name).copied()
    }

    /// The registered names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(String::as_str)
    }
}

/// One logged evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct LoggedTrace {
    /// The binding's site.
    pub site: String,
    /// The app's update count when it was taken.
    pub frame: u64,
    /// The outcome.
    pub outcome: Option<ChooserOutcome>,
    /// The trace.
    pub trace: ChooserTrace,
}

/// The last [`ChooserTraceLog::PER_SITE`] traces of each binding site, when enabled (off by
/// default; enabling it makes bindings evaluate traced, `Brief` detail).
#[derive(Resource, Debug, Default)]
pub struct ChooserTraceLog {
    /// Whether bindings log traces.
    pub enabled: bool,
    sites: BTreeMap<String, VecDeque<LoggedTrace>>,
}

impl ChooserTraceLog {
    /// Traces kept per site.
    pub const PER_SITE: usize = 32;

    /// Adds a trace to its site's ring.
    pub fn push(&mut self, entry: LoggedTrace) {
        let ring = self.sites.entry(entry.site.clone()).or_default();
        ring.push_back(entry);
        while ring.len() > Self::PER_SITE {
            ring.pop_front();
        }
    }

    /// The traces of `site` (every site when `None`), oldest first, at most `limit` per call
    /// (the newest).
    #[must_use]
    pub fn traces(&self, site: Option<&str>, limit: usize) -> Vec<&LoggedTrace> {
        let mut all: Vec<&LoggedTrace> = match site {
            Some(site) => self
                .sites
                .get(site)
                .map(|r| r.iter().collect())
                .unwrap_or_default(),
            None => self.sites.values().flat_map(|r| r.iter()).collect(),
        };
        all.sort_by_key(|t| t.frame);
        let skip = all.len().saturating_sub(limit);
        all.split_off(skip)
    }
}

/// Counters for `debug.stats`-style checks and `wait_until` (no wall time).
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChooserStats {
    /// Binding evaluations.
    pub evaluations: u64,
    /// `ChooserPicked` messages written.
    pub picks: u64,
    /// Binding passes that skipped an unchanged binding.
    pub skipped: u64,
    /// Bindings whose table or provider was missing.
    pub unresolved: u64,
    /// Fixed steps run (the trace log's frame stamp; audit G finding 3, ADR-4756).
    pub frame: u64,
    /// Montage plays the animation bridge started.
    pub montage_plays: u64,
    /// Sound cue plays the audio bridge asked for.
    pub cue_plays: u64,
}

/// The chooser schedule set.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChooserSet {
    /// Binding evaluation (`FixedUpdate`, once per fixed step, audit G finding 3, ADR-4756).
    Evaluate,
    /// The bridges that turn [`ChooserPicked`] into montage and cue plays (`PostUpdate`, before
    /// `PostUpdateSet::AnimationApply` and `PostUpdateSet::AudioSync`, so a pick plays the same frame).
    Bridge,
}

/// The asset paths an app registered with [`ChooserAppExt::register_chooser`].
#[derive(Resource, Debug, Default)]
pub struct ChooserRegistrations {
    paths: Vec<String>,
}

impl ChooserRegistrations {
    /// The registered asset paths, in registration order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.paths.iter().map(String::as_str)
    }

    /// Registers `path` (once).
    pub fn add(&mut self, path: &str) {
        if !self.paths.iter().any(|p| p == path) {
            self.paths.push(path.to_owned());
        }
    }
}

/// Changed chooser files waiting for [`apply_chooser_reloads`].
#[derive(Resource, Debug, Default)]
pub struct PendingChooserReloads(Vec<(String, PathBuf)>);

/// Registers choosers and providers on an [`App`].
pub trait ChooserAppExt {
    /// Loads the table at asset path `path` into [`Choosers`] at startup and reloads it when the
    /// file changes (dev builds; a reload is applied only if it loads). Adds [`ChooserPlugin`]
    /// when it is missing.
    fn register_chooser(&mut self, path: &str) -> &mut Self;

    /// Registers a context provider for [`ChooserBinding::provider`].
    fn register_chooser_provider(&mut self, name: &str, provider: ProviderFn) -> &mut Self;
}

impl ChooserAppExt for App {
    fn register_chooser(&mut self, path: &str) -> &mut Self {
        if !self.world().contains_resource::<Choosers>() {
            self.add_plugins(ChooserPlugin);
        }
        let mut registrations = self.world_mut().resource_mut::<ChooserRegistrations>();
        if !registrations.paths.iter().any(|p| p == path) {
            registrations.paths.push(path.to_owned());
        }
        self
    }

    fn register_chooser_provider(&mut self, name: &str, provider: ProviderFn) -> &mut Self {
        if !self.world().contains_resource::<Choosers>() {
            self.add_plugins(ChooserPlugin);
        }
        self.world_mut()
            .resource_mut::<ChooserProviders>()
            .register(name, provider);
        self
    }
}

/// Adds [`Choosers`], providers, the trace log, stats, [`ChooserPicked`], the startup load, hot
/// reload, and binding evaluation in [`ChooserSet::Evaluate`] (`FixedUpdate`).
pub struct ChooserPlugin;

impl Plugin for ChooserPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Choosers>()
            .init_resource::<ChooserProviders>()
            .init_resource::<ChooserTraceLog>()
            .init_resource::<ChooserStats>()
            .init_resource::<ChooserRegistrations>()
            .init_resource::<PendingChooserReloads>()
            .register_type::<ChooserBinding>()
            .add_message::<ChooserPicked>()
            .add_message::<SourceAssetChanged>()
            .configure_sets(FixedUpdate, ChooserSet::Evaluate)
            .configure_sets(
                PostUpdate,
                ChooserSet::Bridge
                    .after(PostUpdateSet::LateScripts)
                    .before(PostUpdateSet::AnimationApply),
            )
            .add_systems(Startup, load_registered_choosers)
            .add_systems(Update, (collect_chooser_changes, apply_chooser_reloads).chain())
            .add_systems(FixedUpdate, evaluate_bindings.in_set(ChooserSet::Evaluate));
    }
}

fn chooser_file(world: &World, path: &str) -> Option<PathBuf> {
    let server = world.get_resource::<AssetServer>()?;
    // The project file, else the earliest overlay root that holds it (ADR-8845).
    Some(server.source_path(path))
}

/// `Startup`: loads every registered chooser, then re-validates all of them (nested references
/// resolve once every table is in).
pub fn load_registered_choosers(world: &mut World) {
    let paths: Vec<String> = match world.get_resource::<ChooserRegistrations>() {
        Some(registrations) => registrations.paths().map(str::to_owned).collect(),
        None => return,
    };
    for path in &paths {
        let read = read_table(world, path, CHOOSER_TYPE_PATH);
        let file = chooser_file(world, path);
        let Some(mut choosers) = world.get_resource_mut::<Choosers>() else {
            return;
        };
        match read {
            Ok(bytes) => {
                let loaded = match &bytes {
                    TableBytes::Text(text) => choosers.load_text(path, text),
                    TableBytes::Cooked(payload) => choosers.load_cooked(path, payload),
                };
                if let Err(issues) = loaded {
                    for issue in &issues {
                        tracing::error!(target: "regia_framework", chooser = %path, "chooser did not load: {issue}");
                    }
                }
            }
            Err(error) => {
                tracing::error!(target: "regia_framework", chooser = %path, "chooser not read: {error}");
                let slot = choosers.slots.entry(path.clone()).or_default();
                slot.last_issues = vec![ChooserIssue::error("chooser_bad_file", error)];
            }
        }
        if let Some(slot) = choosers.slots.get_mut(path) {
            slot.file = file;
        }
    }
    if let Some(mut choosers) = world.get_resource_mut::<Choosers>() {
        for path in &paths {
            if choosers.generation(path) > 0 {
                choosers.revalidate(path);
            }
        }
    }
}

/// `Update`: collects changed files that are registered choosers.
pub fn collect_chooser_changes(
    mut changes: MessageReader<SourceAssetChanged>,
    registrations: Option<Res<ChooserRegistrations>>,
    pending: Option<ResMut<PendingChooserReloads>>,
) {
    let (Some(registrations), Some(mut pending)) = (registrations, pending) else {
        changes.clear();
        return;
    };
    for change in changes.read() {
        // The longest registered path the file ends with: `anim/hit.chooser.ron` wins over
        // `hit.chooser.ron` for `assets/anim/hit.chooser.ron` (ADR-1809).
        if let Some(path) = registrations
            .paths()
            .filter(|path| change.path.ends_with(Path::new(path)))
            .max_by_key(|path| Path::new(path).components().count())
        {
            pending.0.push((path.to_owned(), change.path.clone()));
        }
    }
}

/// `Update`, after [`collect_chooser_changes`]: re-reads each changed chooser. A file that does
/// not load keeps the old table; its errors go to [`ChooserSlot::last_issues`] (`chooser.list`).
pub fn apply_chooser_reloads(world: &mut World) {
    let pending = match world.get_resource_mut::<PendingChooserReloads>() {
        Some(mut pending) => std::mem::take(&mut pending.0),
        None => return,
    };
    if pending.is_empty() {
        return;
    }
    let _span =
        tracing::info_span!(target: "regia_framework", "chooser_reload", files = pending.len()).entered();
    let mut done: Vec<String> = Vec::new();
    for (path, file) in pending {
        if done.contains(&path) {
            continue;
        }
        done.push(path.clone());
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(target: "regia_framework", chooser = %path, "cannot re-read {}: {error}", file.display());
                continue;
            }
        };
        let Some(mut choosers) = world.get_resource_mut::<Choosers>() else {
            return;
        };
        match choosers.load_text(&path, &text) {
            Ok(generation) => {
                tracing::info!(target: "regia_framework", chooser = %path, generation, "chooser reloaded");
            }
            Err(issues) => {
                for issue in &issues {
                    tracing::error!(target: "regia_framework", chooser = %path, "reload refused: {issue}");
                }
            }
        }
    }
}

/// Whether `b` is the same pick as `a`: the same row, catch-all and result, and, when the table
/// changed since `a` (a reload, an edit or a swapped table), the same outputs too, so an edited
/// `rate` on an unchanged row reaches the bridges (audit-gameplay H7, ADR-1809).
fn same_pick(a: Option<&ChooserOutcome>, b: Option<&ChooserOutcome>, table_changed: bool) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.row == b.row
                && a.used_catch_all == b.used_catch_all
                && a.result == b.result
                && (!table_changed || a.outputs == b.outputs)
        }
        _ => false,
    }
}

/// The random-stream id of a bound entity: its [`EntityGuid`] mixed to 64 bits when it has one
/// (stable across saves, seeks and spawn order; audit G finding 4, ADR-4756), else its entity
/// bits (spawn-order dependent; games give bound entities a guid for determinism). The guid
/// halves pass a splitmix64 finalizer each before the XOR: the bare `hi ^ lo` fold collapsed
/// swapped-half guids (and other `hi ^ lo` collisions) onto one stream (Muse read-only review of
/// fix-framework2-determinism, low finding 6).
///
/// Public so every evaluator of a binding (the fixed-step pass and the agent's
/// `chooser.evaluate_binding`) starts the binding's random stream at the same place (spec 51 § 7).
pub fn binding_stream(world: &World, entity: Entity) -> u64 {
    world.get::<EntityGuid>(entity).map_or_else(
        || entity.to_bits(),
        |guid| {
            let bits = guid.0.0;
            splitmix64(bits as u64) ^ splitmix64((bits >> 64) as u64)
        },
    )
}

thread_local! {
    static PROVIDER_MAP_CLONES: Cell<u64> = const { Cell::new(0) };
    static TABLE_PATH_CLONES: Cell<u64> = const { Cell::new(0) };
}

/// How many times a chooser provider name was copied off the stack on this thread.
///
/// Names that fit in the 128-byte stack key do not count.
#[cfg(test)]
pub(crate) fn provider_map_clones() -> u64 {
    PROVIDER_MAP_CLONES.with(|count| count.get())
}

/// How many times a binding stored a new table path on this thread.
#[cfg(test)]
pub(crate) fn table_path_clones() -> u64 {
    TABLE_PATH_CLONES.with(|count| count.get())
}

/// One context per bound entity, kept across steps so a stable input key is overwritten in place.
#[derive(Resource, Default)]
struct BindingScratch {
    contexts: Vec<(Entity, ChooserContext)>,
}

/// How many scratch contexts are held (tests: they go with their bound entities).
#[cfg(test)]
pub(crate) fn binding_scratch_len(world: &World) -> usize {
    world
        .get_resource::<BindingScratch>()
        .map_or(0, |scratch| scratch.contexts.len())
}

fn scratch_context(contexts: &mut Vec<(Entity, ChooserContext)>, entity: Entity) -> &mut ChooserContext {
    match contexts.binary_search_by_key(&entity, |(id, _)| *id) {
        Ok(index) => &mut contexts[index].1,
        Err(index) => {
            contexts.insert(index, (entity, ChooserContext::default()));
            &mut contexts[index].1
        }
    }
}

/// Provider name copied onto the stack. A name longer than 128 bytes falls back to a `String`
/// and counts toward `provider_map_clones`.
struct ProviderKey {
    buf: [u8; 128],
    len: usize,
    owned: Option<String>,
}

impl ProviderKey {
    fn from_name(name: &str) -> Self {
        if name.len() > 128 {
            PROVIDER_MAP_CLONES.with(|count| count.set(count.get().saturating_add(1)));
            return Self {
                buf: [0; 128],
                len: 0,
                owned: Some(name.to_owned()),
            };
        }
        let mut buf = [0_u8; 128];
        buf[..name.len()].copy_from_slice(name.as_bytes());
        Self {
            buf,
            len: name.len(),
            owned: None,
        }
    }

    fn as_str(&self) -> &str {
        if let Some(owned) = &self.owned {
            return owned.as_str();
        }
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

/// The splitmix64 finalizer (Steele et al., 2014, public domain reference implementation):
/// a bijective, avalanche-mixing 64-bit hash used here to fold a guid half.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// `FixedUpdate` ([`ChooserSet::Evaluate`], once per fixed step): evaluates every
/// [`ChooserBinding`] in entity order. A binding whose table or provider is missing is skipped
/// (counted in [`ChooserStats::unresolved`]).
pub fn evaluate_bindings(world: &mut World) {
    let _span = tracing::info_span!(target: "regia_framework", "chooser_bindings").entered();
    if let Some(mut stats) = world.get_resource_mut::<ChooserStats>() {
        stats.frame += 1;
    }
    let mut entities: Vec<Entity> = world
        .query_filtered::<Entity, With<ChooserBinding>>()
        .iter(world)
        .collect();
    if entities.is_empty() {
        // No bound entity is left: release the contexts of the ones that went (the prune at the
        // end of a pass does not run on this path; corr-framework-a L7).
        if let Some(mut scratch) = world.get_resource_mut::<BindingScratch>()
            && !scratch.contexts.is_empty()
        {
            scratch.contexts = Vec::new();
        }
        return;
    }
    entities.sort();
    let Some(choosers) = world.remove_resource::<Choosers>() else {
        return;
    };
    let mut scratch = world.remove_resource::<BindingScratch>().unwrap_or_default();
    let logging = world
        .get_resource::<ChooserTraceLog>()
        .is_some_and(|log| log.enabled);
    let frame = world.get_resource::<ChooserStats>().map_or(0, |s| s.frame);
    let mut stats = world.get_resource::<ChooserStats>().copied().unwrap_or_default();
    let mut picked: Vec<ChooserPicked> = Vec::new();
    let mut logged: Vec<LoggedTrace> = Vec::new();
    for entity in &entities {
        let entity = *entity;
        let provider_key = {
            let Some(binding) = world.get::<ChooserBinding>(entity) else {
                continue;
            };
            ProviderKey::from_name(&binding.provider)
        };
        let has_table = world
            .get::<ChooserBinding>(entity)
            .is_some_and(|binding| choosers.get(&binding.table).is_some());
        let provider = world
            .get_resource::<ChooserProviders>()
            .and_then(|providers| providers.get(provider_key.as_str()));
        let Some(provider) = provider.filter(|_| has_table) else {
            stats.unresolved += 1;
            continue;
        };
        {
            let ctx = scratch_context(&mut scratch.contexts, entity);
            ctx.begin_fill();
            provider(world, entity, ctx);
            ctx.end_fill();
        }
        let stream = binding_stream(world, entity);
        let (table_changed, inputs_changed, generation) = {
            let Some(binding) = world.get::<ChooserBinding>(entity) else {
                continue;
            };
            let Some(ctx) = scratch
                .contexts
                .iter()
                .find(|(id, _)| *id == entity)
                .map(|(_, ctx)| ctx)
            else {
                continue;
            };
            let generation = choosers.generation(&binding.table);
            let table_changed = binding
                .last_table
                .as_ref()
                .is_none_or(|(path, stored)| path != binding.table.as_str() || *stored != generation);
            let inputs_changed = binding
                .last_context
                .as_ref()
                .is_none_or(|last| !last.same_inputs(ctx));
            (table_changed, inputs_changed, generation)
        };
        let due = {
            let Some(binding) = world.get::<ChooserBinding>(entity) else {
                continue;
            };
            binding.requested
                || match binding.reeval {
                    Reeval::OnChange => table_changed || inputs_changed,
                    Reeval::EveryTick => true,
                    Reeval::Manual => false,
                }
        };
        if !due {
            stats.skipped += 1;
            continue;
        }
        let Some(table) = (|| {
            let binding = world.get::<ChooserBinding>(entity)?;
            choosers.get(binding.table.as_str())
        })() else {
            stats.unresolved += 1;
            continue;
        };
        let Some(mut binding) = world.get_mut::<ChooserBinding>(entity) else {
            continue;
        };
        binding.requested = false;
        if table_changed {
            TABLE_PATH_CLONES.with(|count| count.set(count.get().saturating_add(1)));
            binding.last_table = Some((binding.table.clone(), generation));
        }
        let seed = binding.seed;
        let mut rng = binding
            .rng
            .take()
            .unwrap_or_else(|| Rng::with_stream(seed, stream));
        let mut recent = std::mem::take(&mut binding.recent);
        let ctx = scratch_context(&mut scratch.contexts, entity);
        let outcome = if logging {
            let (outcome, mut trace) =
                table.evaluate_traced(&choosers, ctx, &mut rng, Some(&mut recent), TraceDetail::Brief);
            trace.table.clone_from(&binding.table);
            // A traced evaluation never records recent picks; a binding's pick is real, so record it.
            if table.mode == super::Mode::Weighted
                && let Some(row) = outcome.as_ref().and_then(|o| o.row.as_deref())
            {
                recent.record(row);
            }
            logged.push(LoggedTrace {
                site: binding.site.clone(),
                frame,
                outcome: outcome.clone(),
                trace,
            });
            outcome
        } else {
            table.evaluate(&choosers, ctx, &mut rng, Some(&mut recent))
        };
        stats.evaluations += 1;
        binding.evaluations += 1;
        binding.rng = Some(rng);
        binding.recent = recent;
        if !same_pick(binding.last_pick.as_ref(), outcome.as_ref(), table_changed)
            && let Some(outcome) = &outcome
        {
            picked.push(ChooserPicked {
                entity,
                site: binding.site.clone(),
                table: binding.table.clone(),
                row: outcome.row.clone(),
                result: outcome.result.clone(),
                used_catch_all: outcome.used_catch_all,
                outputs: outcome.outputs.clone(),
            });
            stats.picks += 1;
        }
        // The stored pick is always the latest outcome (its outputs too), picked or not.
        binding.last_pick = outcome;
        binding.last_context = Some(ctx.clone());
    }
    scratch
        .contexts
        .retain(|(entity, _)| entities.binary_search(entity).is_ok());
    world.insert_resource(scratch);
    world.insert_resource(choosers);
    world.insert_resource(stats);
    for message in picked {
        world.write_message(message);
    }
    if !logged.is_empty()
        && let Some(mut log) = world.get_resource_mut::<ChooserTraceLog>()
    {
        for entry in logged {
            log.push(entry);
        }
    }
}
