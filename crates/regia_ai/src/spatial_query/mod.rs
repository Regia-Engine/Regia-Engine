// SPDX-License-Identifier: MPL-2.0
//! Spatial queries for AI (`docs/specs/33-spatial-queries.md`, ADR-1577, ADR-1674): a data-driven
//! way to ask "where should I stand" or "which pickup should I take".
//!
//! A `.spatialquery.ron` asset ([`SpatialQueryAsset`]) names a *generator* that produces candidate
//! items (points or entities), *tests* that drop items failing a condition and score the rest, and a
//! *pick* rule. [`SpatialQueryManager`] (a resource installed by [`SpatialQueryPlugin`]) runs
//! requests either at once ([`SpatialQueryManager::run_instant`]) or sliced across fixed ticks under
//! a work-unit budget ([`SpatialQueryManager::submit`] / [`SpatialQueryManager::poll`]). Behavior
//! trees use the [`RunSpatialQuery`] leaf; editors and agents preview a query with a
//! [`SpatialQueryProbe`] entity.
//!
//! An instant run (`run_instant`) is a pure function of (one world snapshot, template, parameters, seed); an
//! async run (`submit`) evaluates later items on later fixed steps and sees the world as it is at each step: iteration follows
//! generation order, score ties go to the lower generation index, and randomness (the `Sample`
//! pick) comes only from the request seed through `regia_core::math::Rng`. Work is counted in units,
//! never timed; wall-clock time appears only in the tracing spans `spatial_query_tick`,
//! `spatial_query_generate` and `spatial_query_test`.
//!
//! See `book/agents/spatial-queries.md`.

mod asset;
mod bt;
mod exec;
mod probe;
mod score;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

pub use asset::{
    Anchor, ArcSpec, CastShape, Clamp, Combine, CustomValue, DEFAULT_MAX_TESTS, Direction, DistanceMode,
    Generator, ItemType, Num, Origin, ParamValue, Pick, Purpose, Require, RouteDirection, RouteMeasure,
    SPATIAL_QUERY_FORMAT_VERSION, SPATIAL_QUERY_MIN_FORMAT_VERSION, ScoreSpec, Shape, SnapTarget,
    SpatialQueryAsset, SpatialQueryLoadError, Test, ZoneMode, migrate_ron_v1_to_v2,
    spatial_query_migration_sources, spatial_query_schema_fingerprint,
};
pub use bt::{BtParam, RunSpatialQuery};
pub use exec::{
    AnchorData, AnchorProvider, GeneratedItem, GeneratorImpl, GeneratorInput, TagMatcher, TestImpl,
    TestInput, TestValue,
};
#[cfg(feature = "debug_draw")]
pub use probe::SpatialQueryGizmos;
pub use probe::{PROBE_RERUN_DISTANCE, ProbeHighlight, SpatialQueryProbe, SpatialQueryProbeState};
pub use score::{ScoreCurve, apply_shape, combine};

use exec::{Env, Run, RunSpec, Step};
use regia_app::prelude::*;
use regia_core::math::Vec3;
#[cfg(feature = "debug_draw")]
use regia_render::gizmos::AppGizmoBuilder;
use regia_scene::components::Tags;

/// The id of one request, unique per manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u64);

/// How a request ended (or that it is still running). The agent string is [`QueryStatus::name`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryStatus {
    /// Still running (async).
    Processing,
    /// Finished with a result (`All` may hold zero items).
    Success,
    /// No survivor for `Best`/`Sample`, a missing backing system, too many items, a bad parameter,
    /// or world teardown.
    Failed,
    /// Aborted by the caller (or stopped at a probe's `max_units`).
    Aborted,
    /// The owner was despawned before the request finished.
    OwnerLost,
    /// A parameter is used but neither declared nor supplied.
    MissingParam,
}

impl QueryStatus {
    /// The agent string: `processing`, `success`, `failed`, `aborted`, `owner_lost`, `missing_param`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Processing => "processing",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
            Self::OwnerLost => "owner_lost",
            Self::MissingParam => "missing_param",
        }
    }
}

/// One item of a result.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredItem {
    /// World position (after any snap).
    pub position: Vec3,
    /// The entity of an entity item.
    pub entity: Option<Entity>,
    /// The normalized score, -1..1 (0 without score tests).
    pub score: f32,
    /// Survived every filter.
    pub passed: bool,
    /// The id of the filter that dropped the item (`keep_failed` only).
    pub failed_test: Option<String>,
    /// Why: `filter`, `no_anchor`, `snap` or `unreachable`.
    pub failed_reason: Option<String>,
    /// Each score test's contribution (`weight * shaped value`), by test id.
    pub contributions: Vec<(String, f32)>,
    /// The item's index in generation order.
    pub generation_index: usize,
}

/// How many items entered and left one test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestSummary {
    /// The test id.
    pub id: String,
    /// The test variant name.
    pub kind: &'static str,
    /// Items alive when the test started.
    pub input: usize,
    /// Items alive after it.
    pub output: usize,
}

/// The result of one request.
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialQueryResult {
    /// How it ended.
    pub status: QueryStatus,
    /// The picked items (one for `Best`/`Sample`, all survivors for `All`), then the failed items
    /// when `keep_failed` was set.
    pub items: Vec<ScoredItem>,
    /// Per-test counts, in authoring order.
    pub tests: Vec<TestSummary>,
    /// How many items survived the filters.
    pub item_count: usize,
    /// Work units spent.
    pub units: u64,
    /// Why the request did not succeed (names a missing parameter or system).
    pub message: Option<String>,
    /// The agent error code of a failure (`sq_missing_system`, `sq_too_many_items`, `sq_no_navmesh`,
    /// `sq_bad_param`), or `None`.
    pub code: Option<&'static str>,
}

impl SpatialQueryResult {
    /// The best (first) item, if any.
    pub fn best(&self) -> Option<&ScoredItem> {
        self.items.first().filter(|i| i.passed)
    }

    fn ended(status: QueryStatus, message: impl Into<String>, code: Option<&'static str>) -> Self {
        Self {
            status,
            items: Vec::new(),
            tests: Vec::new(),
            item_count: 0,
            units: 0,
            message: Some(message.into()),
            code,
        }
    }
}

/// One execution request (§ 4).
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialQueryRequest {
    /// A loaded template name, or a path (relative to [`SpatialQueryManager::asset_root`]).
    pub template: String,
    /// The asker.
    pub owner: Entity,
    /// Parameter overrides by name.
    pub params: BTreeMap<String, ParamValue>,
    /// The pick (`None`: the asset's `default_pick`).
    pub pick: Option<Pick>,
    /// The seed for `Sample` (`None`: derived from the owner's spawn-order arrival and the
    /// request id, never from the owner's entity bits, which two runs of the same level can
    /// allocate differently; the blackboard's arrival decides while the tree tick holds the
    /// `BehaviorTree` out of the owner, the behavior tree's otherwise, and an owner the
    /// tracking never saw falls back to its entity key; an explicit seed wins over both).
    pub seed: Option<u64>,
    /// Also return the items that failed a filter (debug).
    pub keep_failed: bool,
}

impl SpatialQueryRequest {
    /// A request for `template` asked by `owner` with defaults.
    pub fn new(template: impl Into<String>, owner: Entity) -> Self {
        Self {
            template: template.into(),
            owner,
            params: BTreeMap::new(),
            pick: None,
            seed: None,
            keep_failed: false,
        }
    }

    /// Overrides a parameter.
    pub fn with_param(mut self, name: impl Into<String>, value: ParamValue) -> Self {
        self.params.insert(name.into(), value);
        self
    }

    /// Sets the pick.
    pub fn with_pick(mut self, pick: Pick) -> Self {
        self.pick = Some(pick);
        self
    }

    /// Sets the seed.
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }
}

/// Limits (§ 3.6).
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialQueryConfig {
    /// Generated items per request (20,000).
    pub max_items: usize,
    /// Tests per asset (32).
    pub max_tests: usize,
    /// Work units the async tick may spend (4,096).
    pub work_units_per_tick: u64,
    /// Warn when more requests than this are active (64).
    pub active_warn_threshold: usize,
    /// Warn when one request spends more than this many units (200,000).
    pub slow_warn_units: u64,
    /// Breadth first (every active request advances by one stage per round) or oldest first.
    pub breadth_first: bool,
}

impl Default for SpatialQueryConfig {
    fn default() -> Self {
        Self {
            max_items: 20_000,
            max_tests: DEFAULT_MAX_TESTS,
            work_units_per_tick: 4_096,
            active_warn_threshold: 64,
            slow_warn_units: 200_000,
            breadth_first: true,
        }
    }
}

/// Counters for `ai.query_stats`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpatialQueryStats {
    /// Requests running now.
    pub active: usize,
    /// Requests submitted (async) or run (instant) so far.
    pub submitted: u64,
    /// Requests that ended `success`.
    pub completed: u64,
    /// Requests aborted.
    pub aborted: u64,
    /// Requests that ended `failed`, `owner_lost` or `missing_param`.
    pub failed: u64,
    /// Units the last async tick spent.
    pub units_last_tick: u64,
    /// The most recent warnings (at most 16).
    pub warnings: Vec<String>,
}

/// A loaded template.
#[derive(Debug, Clone)]
pub struct LoadedTemplate {
    /// The asset.
    pub asset: Arc<SpatialQueryAsset>,
    /// The file it came from (`None` for a template registered from Rust).
    pub path: Option<String>,
    modified: Option<SystemTime>,
}

/// Why a template lookup failed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TemplateError {
    /// No loaded template has the name and no file exists at the path (`sq_unknown_template`).
    #[error("no spatial-query template `{0}`")]
    Unknown(String),
    /// The file did not load.
    #[error("{path}: {error}")]
    Load {
        /// The path.
        path: String,
        /// The load error.
        error: SpatialQueryLoadError,
    },
}

impl TemplateError {
    /// The agent error code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unknown(_) => "sq_unknown_template",
            Self::Load { error, .. } => error.code(),
        }
    }
}

struct Active {
    run: Run,
    warned: bool,
}

/// Fixed ticks a finished result whose owner is gone waits for a poll before it is dropped
/// (ten seconds at 60 Hz): nothing else would ever take it (ADR-2396).
const FINISHED_DEAD_OWNER_GRACE_TICKS: u64 = 600;

/// A result waiting for [`SpatialQueryManager::poll`], with the owner and the tick it finished on.
struct Finished {
    result: SpatialQueryResult,
    owner: Entity,
    tick: u64,
}

/// The spatial-query resource: templates, anchor providers, the tag matcher and the active requests.
#[derive(Resource)]
pub struct SpatialQueryManager {
    /// Limits.
    pub config: SpatialQueryConfig,
    /// The directory template paths are relative to (default: the working directory).
    pub asset_root: PathBuf,
    templates: BTreeMap<String, LoadedTemplate>,
    paths: BTreeMap<String, String>,
    providers: BTreeMap<String, Box<dyn AnchorProvider>>,
    generators: BTreeMap<String, Box<dyn GeneratorImpl>>,
    tests: BTreeMap<String, Box<dyn TestImpl>>,
    matcher: Option<(String, TagMatcher)>,
    active: Vec<Active>,
    finished: BTreeMap<RequestId, Finished>,
    ticks: u64,
    next_id: u64,
    stats: SpatialQueryStats,
}

impl Default for SpatialQueryManager {
    fn default() -> Self {
        Self {
            config: SpatialQueryConfig::default(),
            asset_root: PathBuf::from("."),
            templates: BTreeMap::new(),
            paths: BTreeMap::new(),
            providers: BTreeMap::new(),
            generators: BTreeMap::new(),
            tests: BTreeMap::new(),
            matcher: None,
            active: Vec::new(),
            finished: BTreeMap::new(),
            ticks: 0,
            next_id: 1,
            stats: SpatialQueryStats::default(),
        }
    }
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The stable part of a request's default seed: the owner's arrival number among behavior trees
/// (a `RunSpatialQuery` leaf asks on a tree's entity), never its entity bits — `Commands::spawn`
/// reserves ids in record-time thread order, so the same logical owner gets different bits in two
/// runs of the same level. An owner the tracking never saw falls back to its entity key, and an
/// explicit seed wins over both.
///
/// While a leaf runs, the tree tick holds the `BehaviorTree` out of the owner
/// (take-run-reinsert), so the behavior-tree table has no entry for it: the blackboard's
/// arrival decides first — the tick never takes that component.
fn owner_seed_key(world: &World, owner: Entity) -> u64 {
    if let Some(order) = world.get_resource::<regia_ecs::spawn_order::SpawnOrder<crate::Blackboard>>()
        && let Some(seq) = order.get(owner)
    {
        return seq;
    }
    match world.get_resource::<regia_ecs::spawn_order::SpawnOrder<crate::BehaviorTree>>() {
        Some(order) => match order.get(owner) {
            Some(seq) => seq,
            None => {
                // `SpawnOrder::key` for an unseen entity is `(u64::MAX, index, bits)`. The seed
                // needs one `u64`. The low half of `to_bits` is the niche encoding of the index
                // (its bitwise complement), so XORing the raw index back in is all-ones for every
                // entity and every owner of one generation shared a key.
                let (_, _, bits) = order.key(owner);
                bits
            }
        },
        None => owner.to_bits(),
    }
}

impl SpatialQueryManager {
    /// Registers a template built in Rust under its `name`, replacing one with the same name.
    pub fn register_template(&mut self, asset: SpatialQueryAsset) -> String {
        let name = asset.name.clone();
        self.templates.insert(
            name.clone(),
            LoadedTemplate {
                asset: Arc::new(asset),
                path: None,
                modified: None,
            },
        );
        name
    }

    /// Parses and registers a template from RON text.
    ///
    /// # Errors
    ///
    /// The text does not load.
    pub fn register_template_text(&mut self, text: &str) -> Result<String, SpatialQueryLoadError> {
        let asset = SpatialQueryAsset::parse_with_limit(text, self.config.max_tests)?;
        Ok(self.register_template(asset))
    }

    /// Loads (or reloads) the template file at `path` (relative to [`Self::asset_root`]).
    ///
    /// # Errors
    ///
    /// The file is missing ([`TemplateError::Unknown`]) or does not load.
    pub fn load_template(&mut self, path: &str) -> Result<String, TemplateError> {
        let full = self.full_path(path);
        // Stat before reading: a save between the read and a later stat would pair the old text
        // with the new mtime and the template would never reload (ADR-2396).
        let modified = std::fs::metadata(&full).and_then(|m| m.modified()).ok();
        let text = std::fs::read_to_string(&full).map_err(|_| TemplateError::Unknown(path.to_string()))?;
        let asset = SpatialQueryAsset::parse_with_limit(&text, self.config.max_tests).map_err(|error| {
            TemplateError::Load {
                path: path.to_string(),
                error,
            }
        })?;
        let name = asset.name.clone();
        self.templates.insert(
            name.clone(),
            LoadedTemplate {
                asset: Arc::new(asset),
                path: Some(path.to_string()),
                modified,
            },
        );
        self.paths.insert(path.to_string(), name.clone());
        Ok(name)
    }

    fn full_path(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.asset_root.join(p)
        }
    }

    /// Resolves a template: a loaded name, then a loaded path, then a file. A file-backed template
    /// whose file changed since it loaded is reloaded; running requests keep the version they started
    /// with.
    ///
    /// # Errors
    ///
    /// See [`TemplateError`].
    pub fn template(&mut self, template: &str) -> Result<Arc<SpatialQueryAsset>, TemplateError> {
        let name = if self.templates.contains_key(template) {
            Some(template.to_string())
        } else {
            self.paths.get(template).cloned()
        };
        if let Some(name) = name
            && let Some(loaded) = self.templates.get(&name)
        {
            let stale = loaded.path.as_ref().is_some_and(|path| {
                let now = std::fs::metadata(self.full_path(path))
                    .and_then(|m| m.modified())
                    .ok();
                now.is_some() && now != loaded.modified
            });
            if !stale {
                return Ok(Arc::clone(&loaded.asset));
            }
            if let Some(path) = loaded.path.clone() {
                let reloaded = self.load_template(&path)?;
                if let Some(loaded) = self.templates.get(&reloaded) {
                    return Ok(Arc::clone(&loaded.asset));
                }
            }
        }
        let name = self.load_template(template)?;
        self.templates
            .get(&name)
            .map(|loaded| Arc::clone(&loaded.asset))
            .ok_or_else(|| TemplateError::Unknown(template.to_string()))
    }

    /// The loaded templates by name.
    pub fn templates(&self) -> impl Iterator<Item = (&str, &LoadedTemplate)> {
        self.templates.iter().map(|(name, t)| (name.as_str(), t))
    }

    /// Registers a Rust anchor provider (`Provider("name")`), replacing one with the same name.
    pub fn register_provider(&mut self, name: &str, provider: Box<dyn AnchorProvider>) {
        self.providers.insert(name.to_string(), provider);
    }

    /// The registered provider names, sorted.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.keys().map(String::as_str).collect()
    }

    /// Registers a Rust generator (`Custom(name: "...")` in an asset's `generator`), replacing one with
    /// the same name.
    pub fn register_generator(&mut self, name: &str, generator: Box<dyn GeneratorImpl>) {
        self.generators.insert(name.to_string(), generator);
    }

    /// The registered generator names, sorted.
    pub fn generator_names(&self) -> Vec<&str> {
        self.generators.keys().map(String::as_str).collect()
    }

    /// Registers a Rust test (`Custom(name: "...")` in an asset's `tests`), replacing one with the same
    /// name.
    pub fn register_test(&mut self, name: &str, test: Box<dyn TestImpl>) {
        self.tests.insert(name.to_string(), test);
    }

    /// The registered test names, sorted.
    pub fn test_names(&self) -> Vec<&str> {
        self.tests.keys().map(String::as_str).collect()
    }

    /// Sets the tag matcher `TagMatch` tests call (a host callback; `name` is reported by the agent).
    pub fn set_tag_matcher(&mut self, name: &str, matcher: TagMatcher) {
        self.matcher = Some((name.to_string(), matcher));
    }

    /// The tag matcher's name, if one is set.
    pub fn tag_matcher_name(&self) -> Option<&str> {
        self.matcher.as_ref().map(|(name, _)| name.as_str())
    }

    fn next_request(&mut self) -> RequestId {
        let id = RequestId(self.next_id);
        self.next_id += 1;
        id
    }

    fn build_run(
        &mut self,
        req: SpatialQueryRequest,
        through_test: Option<usize>,
        max_units: u64,
        owner_key: Option<u64>,
    ) -> Result<(RequestId, Run), (RequestId, SpatialQueryResult)> {
        let id = self.next_request();
        let asset = match self.template(&req.template) {
            Ok(asset) => asset,
            Err(error) => {
                let code = error.code();
                return Err((
                    id,
                    SpatialQueryResult::ended(QueryStatus::Failed, error.to_string(), Some(code)),
                ));
            }
        };
        let pick = req.pick.unwrap_or(asset.default_pick);
        // An explicit seed wins. The default mixes in the owner's spawn-order arrival and the
        // request id; a submission has no world to read the arrival from, so the default stays
        // unresolved until the first tick (`seed_pending`).
        let (seed, seed_pending) = match (req.seed, owner_key) {
            (Some(seed), _) => (seed, false),
            (None, Some(key)) => (splitmix(key ^ splitmix(id.0)), false),
            (None, None) => (0, true),
        };
        let run = Run::new(RunSpec {
            id,
            owner: req.owner,
            asset,
            overrides: req.params,
            pick,
            seed,
            seed_pending,
            keep_failed: req.keep_failed,
            max_items: self.config.max_items,
            through_test,
            max_units,
        });
        Ok((id, run))
    }

    fn count(&mut self, result: &SpatialQueryResult) {
        match result.status {
            QueryStatus::Success => self.stats.completed += 1,
            QueryStatus::Aborted => self.stats.aborted += 1,
            QueryStatus::Processing => {}
            QueryStatus::Failed | QueryStatus::OwnerLost | QueryStatus::MissingParam => {
                self.stats.failed += 1
            }
        }
    }

    fn warn(&mut self, message: String) {
        tracing::warn!(target: "regia_ai", "{message}");
        if self.stats.warnings.len() >= 16 {
            self.stats.warnings.remove(0);
        }
        self.stats.warnings.push(message);
    }

    /// Runs a request to the end now, ignoring the per-tick budget (§ 4).
    pub fn run_instant(&mut self, world: &mut World, req: SpatialQueryRequest) -> SpatialQueryResult {
        self.run_limited(world, req, None, 0)
    }

    /// [`Self::run_instant`] restricted to the tests up to authoring index `through_test` and stopped
    /// (`aborted`) past `max_units` units (0 = no limit). The probe uses it.
    pub fn run_limited(
        &mut self,
        world: &mut World,
        req: SpatialQueryRequest,
        through_test: Option<usize>,
        max_units: u64,
    ) -> SpatialQueryResult {
        let _span = tracing::debug_span!(target: "regia_ai", "spatial_query_instant").entered();
        self.stats.submitted += 1;
        let owner = req.owner;
        let owner_key = owner_seed_key(world, owner);
        let mut run = match self.build_run(req, through_test, max_units, Some(owner_key)) {
            Ok((_, run)) => run,
            Err((_, result)) => {
                self.count(&result);
                return result;
            }
        };
        let result = if world.get_entity(owner).is_err() {
            run.end(
                QueryStatus::OwnerLost,
                Some("the owner entity does not exist".to_string()),
            )
        } else {
            let env = Env {
                world,
                providers: &self.providers,
                generators: &self.generators,
                tests: &self.tests,
                matcher: self.matcher.as_ref().map(|(_, m)| *m),
            };
            let mut budget = u64::MAX;
            loop {
                if run.step(&env, &mut budget) == Step::Finished {
                    break;
                }
            }
            run.take_result().unwrap_or_else(|| {
                SpatialQueryResult::ended(QueryStatus::Failed, "the run ended without a result", None)
            })
        };
        if result.units > self.config.slow_warn_units {
            self.warn(format!(
                "spatial query `{}` spent {} work units (slow_warn_units {})",
                run.spec.asset.name, result.units, self.config.slow_warn_units
            ));
        }
        self.count(&result);
        result
    }

    /// Queues a request; it advances on [`Self::tick`]. A template that does not resolve finishes
    /// `failed` at once (poll it to read the error).
    pub fn submit(&mut self, req: SpatialQueryRequest) -> RequestId {
        self.stats.submitted += 1;
        let owner = req.owner;
        match self.build_run(req, None, 0, None) {
            Ok((id, run)) => {
                self.active.push(Active { run, warned: false });
                if self.active.len() > self.config.active_warn_threshold {
                    let n = self.active.len();
                    self.warn(format!(
                        "{n} spatial queries are active (active_warn_threshold {})",
                        self.config.active_warn_threshold
                    ));
                }
                id
            }
            Err((id, result)) => {
                self.count(&result);
                self.finish(id, owner, result);
                id
            }
        }
    }

    /// The result of a finished request (taken: a second poll answers `None`). `None` while the
    /// request is processing or for an unknown id; see [`Self::is_active`].
    pub fn poll(&mut self, id: RequestId) -> Option<SpatialQueryResult> {
        self.finished.remove(&id).map(|finished| finished.result)
    }

    fn finish(&mut self, id: RequestId, owner: Entity, result: SpatialQueryResult) {
        let tick = self.ticks;
        self.finished.insert(id, Finished { result, owner, tick });
    }

    /// Whether the request is still processing.
    pub fn is_active(&self, id: RequestId) -> bool {
        self.active.iter().any(|a| a.run.spec.id == id)
    }

    /// Work units a processing request has spent so far (`None` once it finished or for an unknown id).
    pub fn units_spent(&self, id: RequestId) -> Option<u64> {
        self.active
            .iter()
            .find(|a| a.run.spec.id == id)
            .map(|a| a.run.units)
    }

    /// Whether the request has a result waiting for [`Self::poll`].
    pub fn is_finished(&self, id: RequestId) -> bool {
        self.finished.contains_key(&id)
    }

    fn end_active(&mut self, index: usize, status: QueryStatus, message: &str) {
        let mut active = self.active.remove(index);
        let result = active.run.end(status, Some(message.to_string()));
        self.count(&result);
        self.finish(active.run.spec.id, active.run.spec.owner, result);
    }

    /// Aborts a processing request (its result reads `aborted`). `false` for a finished or unknown id.
    pub fn abort(&mut self, id: RequestId) -> bool {
        match self.active.iter().position(|a| a.run.spec.id == id) {
            Some(index) => {
                self.end_active(index, QueryStatus::Aborted, "aborted by the caller");
                true
            }
            None => false,
        }
    }

    /// Aborts every processing request of `owner`; returns how many.
    pub fn abort_owner(&mut self, owner: Entity) -> usize {
        let mut count = 0;
        while let Some(index) = self.active.iter().position(|a| a.run.spec.owner == owner) {
            self.end_active(index, QueryStatus::Aborted, "aborted by the caller");
            count += 1;
        }
        count
    }

    /// Drops everything `owner` still has here: processing requests end (`aborted`) and every
    /// finished result of the owner nobody will poll is discarded. Call it when the owner is
    /// despawned (the tree drivers do); returns how many results were discarded.
    pub fn forget_owner(&mut self, owner: Entity) -> usize {
        self.abort_owner(owner);
        let ids: Vec<RequestId> = self
            .finished
            .iter()
            .filter(|(_, finished)| finished.owner == owner)
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            self.finished.remove(id);
        }
        ids.len()
    }

    /// World teardown: every active request ends `failed`.
    pub fn fail_all(&mut self) {
        while !self.active.is_empty() {
            self.end_active(0, QueryStatus::Failed, "the world was torn down");
        }
    }

    /// The counters.
    pub fn stats(&self) -> SpatialQueryStats {
        SpatialQueryStats {
            active: self.active.len(),
            ..self.stats.clone()
        }
    }

    /// Advances the active requests by at most `config.work_units_per_tick` units (§ 4: breadth first
    /// in submit order by default). Requests whose owner was despawned end `owner_lost`.
    pub fn tick(&mut self, world: &World) {
        let _span = tracing::debug_span!(target: "regia_ai", "spatial_query_tick").entered();
        self.ticks += 1;
        // A result nobody can poll any more (its owner is gone) is dropped after a grace period,
        // so owner churn cannot grow the table for the whole session (ADR-2396).
        let now = self.ticks;
        self.finished.retain(|_, finished| {
            now.saturating_sub(finished.tick) <= FINISHED_DEAD_OWNER_GRACE_TICKS
                || world.get_entity(finished.owner).is_ok()
        });
        let mut index = 0;
        while index < self.active.len() {
            if world.get_entity(self.active[index].run.spec.owner).is_err() {
                self.end_active(index, QueryStatus::OwnerLost, "the owner was despawned");
            } else {
                index += 1;
            }
        }
        let limit = self.config.work_units_per_tick;
        // A submitted request had no world when it arrived: its default seed resolves here, from
        // the owner's spawn-order arrival (see `owner_seed_key`), before anything can draw on it.
        for active in &mut self.active {
            if active.run.spec.seed_pending {
                let key = owner_seed_key(world, active.run.spec.owner);
                active.run.spec.seed = splitmix(key ^ splitmix(active.run.spec.id.0));
                active.run.spec.seed_pending = false;
            }
        }
        let mut budget = limit;
        let matcher = self.matcher.as_ref().map(|(_, m)| *m);
        let mut done: Vec<usize> = Vec::new();
        {
            let env = Env {
                world,
                providers: &self.providers,
                generators: &self.generators,
                tests: &self.tests,
                matcher,
            };
            if self.config.breadth_first {
                loop {
                    let mut progressed = false;
                    let mut waiting = (self.active.len() - done.len()) as u64;
                    for (i, active) in self.active.iter_mut().enumerate() {
                        if done.contains(&i) {
                            continue;
                        }
                        if budget == 0 {
                            break;
                        }
                        // An even share of what is left this round: one stage larger than the
                        // whole budget cannot starve the requests behind it (ADR-2396). Units a
                        // request leaves (its stage ended) go to the next ones.
                        let mut share = budget.div_ceil(waiting.max(1));
                        waiting = waiting.saturating_sub(1);
                        let given = share;
                        let step = active.run.step(&env, &mut share);
                        let spent = given - share;
                        budget -= spent;
                        if spent > 0 {
                            progressed = true;
                        }
                        match step {
                            Step::Finished => {
                                done.push(i);
                                progressed = true;
                            }
                            Step::StageDone => progressed = true,
                            Step::Paused => {}
                        }
                    }
                    if budget == 0 || !progressed {
                        break;
                    }
                }
            } else {
                for (i, active) in self.active.iter_mut().enumerate() {
                    loop {
                        if budget == 0 {
                            break;
                        }
                        match active.run.step(&env, &mut budget) {
                            Step::Finished => {
                                done.push(i);
                                break;
                            }
                            Step::StageDone => {}
                            Step::Paused => break,
                        }
                    }
                    if budget == 0 {
                        break;
                    }
                }
            }
        }
        self.stats.units_last_tick = limit - budget;
        let slow = self.config.slow_warn_units;
        let mut slow_warnings = Vec::new();
        for active in &mut self.active {
            if !active.warned && active.run.units > slow {
                active.warned = true;
                slow_warnings.push(format!(
                    "spatial query `{}` has spent {} work units (slow_warn_units {slow})",
                    active.run.spec.asset.name, active.run.units
                ));
            }
        }
        for message in slow_warnings {
            self.warn(message);
        }
        done.sort_unstable();
        for i in done.into_iter().rev() {
            let mut active = self.active.remove(i);
            let result = active.run.take_result().unwrap_or_else(|| {
                SpatialQueryResult::ended(QueryStatus::Failed, "the run ended without a result", None)
            });
            self.count(&result);
            self.finish(active.run.spec.id, active.run.spec.owner, result);
        }
    }
}

/// Installs spatial queries: the [`SpatialQueryManager`] resource, the async tick in `FixedUpdate`
/// (before [`crate::AiStep`], so a behavior tree reads results the same step), the
/// [`RunSpatialQuery`] reflection, the probe types and the `"spatial_query"` gizmo group (off by
/// default). The probe system is compiled only with the `spatial-query-probe` feature (on when the
/// agent is linked; off in shipping builds).
#[derive(Default)]
pub struct SpatialQueryPlugin;

impl Plugin for SpatialQueryPlugin {
    fn build(&self, app: &mut App) {
        // The entity generators and anchors emit items in tag-arrival order (see `sort_entities`);
        // ids are record-time allocations and would split two runs of the same level. Name sets
        // order by the `Name` arrival instead: a `NearbyEntities(name: ...)` match never had to
        // have a `Tags`.
        regia_ecs::spawn_order::track_spawn_order::<Tags>(app.world_mut());
        regia_ecs::spawn_order::track_spawn_order::<regia_scene::components::Name>(app.world_mut());
        // The default `Sample` seed mixes in the owner's arrival among behavior trees
        // (`owner_seed_key`); AiPlugin usually installs this already, and a second call is a no-op.
        // The blackboard's arrival is the seed while a leaf runs (the tree tick takes
        // `BehaviorTree` out of the owner): AiPlugin installs that too; this call covers worlds
        // that use the manager without AiPlugin.
        regia_ecs::spawn_order::track_spawn_order::<crate::BehaviorTree>(app.world_mut());
        regia_ecs::spawn_order::track_spawn_order::<crate::Blackboard>(app.world_mut());
        app.init_resource::<SpatialQueryManager>()
            .register_type::<SpatialQueryProbe>()
            .register_type::<ProbeHighlight>()
            .register_type::<Pick>()
            .register_type::<ParamValue>()
            // After the physics step too: the tests read `PhysicsWorld`, so a wall spawned or moved
            // this step must be in it whatever order the executor picks (ADR-2396).
            .add_systems(
                FixedUpdate,
                tick_spatial_queries
                    .after(regia_physics::PhysicsStep)
                    .before(crate::AiStep),
            );
        #[cfg(feature = "debug_draw")]
        app.add_systems(
            PostUpdate,
            probe::draw_probes
                .run_if(regia_render::gizmos::gizmo_group_enabled::<SpatialQueryGizmos>)
                .after(PostUpdateSet::TransformPropagate),
        );
        #[cfg(feature = "spatial-query-probe")]
        app.add_systems(
            FixedUpdate,
            probe::run_probes
                .after(tick_spatial_queries)
                .before(crate::AiStep),
        );
    }

    #[cfg(feature = "debug_draw")]
    fn finish(&self, app: &mut App) {
        // The gizmo group hands its lines to the renderer; register it only when the renderer's gizmo
        // plugin is present, so headless apps (servers, tests) run without it.
        if app.has_plugin(std::any::type_name::<regia_render::gizmos::GizmoPlugin>()) {
            app.init_gizmo_group::<SpatialQueryGizmos>();
        }
    }
}

/// `FixedUpdate` (exclusive): [`SpatialQueryManager::tick`].
pub fn tick_spatial_queries(world: &mut World) {
    world.try_resource_scope(|world, mut manager: Mut<SpatialQueryManager>| manager.tick(world));
}
