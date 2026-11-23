// SPDX-License-Identifier: MPL-2.0
//! One spatial-query execution (`docs/specs/33-spatial-queries.md` §§ 3.2-3.6): resolve anchors,
//! generate, filter, score, pick. The run is a small state machine that advances by work units, so
//! the manager can slice it across fixed ticks (async) or run it to the end (`run_instant`).
//!
//! A *work unit* is one generated item (its projection, ground trace, reach check and dedupe) or one
//! (item, test, anchor value) evaluation. Units are counted, never timed.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

#[cfg(test)]
thread_local! {
    static DEDUPE_COMPARES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Position comparisons the last generate-stage dedupe performed, then resets the counter.
#[cfg(test)]
pub(crate) fn take_dedupe_compares() -> usize {
    DEDUPE_COMPARES.with(|cell| cell.replace(0))
}

#[cfg(test)]
thread_local! {
    static SCAN_RESOLVES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Candidate anchor resolutions the `RouteAll` default `scan_limit` pre-pass performed, then resets
/// the counter.
#[cfg(test)]
pub(crate) fn take_scan_resolves() -> usize {
    SCAN_RESOLVES.with(|cell| cell.replace(0))
}

use regia_app::prelude::*;
use regia_core::math::{Affine3A, Rng, Vec3};
use regia_physics::{
    Collider, ColliderShape, PhysicsLayers, PhysicsWorld, QueryFilter, ShapeAxis, WorldQueries,
};
use regia_scene::components::{Name, Tags};
use regia_scene::{GlobalTransform, Transform};

use super::asset::{
    Anchor, ArcSpec, CastShape, Clamp, CustomValue, Direction, DistanceMode, Generator, ItemType, Num,
    ParamValue, Pick, Require, RouteDirection, RouteMeasure, SnapTarget, SpatialQueryAsset, Test, ZoneMode,
};
use super::score::{ScoreCurve, combine};
use super::{QueryStatus, RequestId, ScoredItem, SpatialQueryResult, TestSummary};
use crate::blackboard::{BbValue, Blackboard};

/// Positions and entities a Rust [`AnchorProvider`] yields.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnchorData {
    /// World positions.
    pub points: Vec<Vec3>,
    /// Entities (their positions are read from their transforms).
    pub entities: Vec<Entity>,
}

/// A Rust anchor registered by name on the manager (`Provider("name")` in an asset).
pub trait AnchorProvider: Send + Sync {
    /// The anchor's values for `asker`. Must be a pure function of the world for determinism.
    fn provide(&self, world: &World, asker: Entity) -> AnchorData;
}

/// What a [`GeneratorImpl`] reads. Everything a custom generator may depend on is here, so the run
/// stays a pure function of (world snapshot, template, parameters, seed).
pub struct GeneratorInput<'a> {
    /// The world (read only).
    pub world: &'a World,
    /// The entity running the query.
    pub asker: Entity,
    /// The values of the row's `center` anchor (possibly none).
    pub centers: &'a [Vec3],
    /// The row's `args`, with parameter references resolved.
    pub args: &'a BTreeMap<String, f32>,
    /// Every parameter of the request: declared defaults overridden by the caller's values.
    pub params: &'a BTreeMap<String, ParamValue>,
    /// The request seed: the only source of randomness a generator may use.
    pub seed: u64,
}

/// One item a [`GeneratorImpl`] produces.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GeneratedItem {
    /// A position, for a `Point` query. Passes through the row's projection and ground trace.
    Point(Vec3),
    /// An entity, for an `Entity` query. Its position comes from its transform; an entity with none
    /// is dropped.
    Entity(Entity),
}

/// A Rust generator registered by name on the manager (`Custom(name: "...")` in an asset).
pub trait GeneratorImpl: Send + Sync {
    /// The candidates, in the order that becomes their generation order. Must be a pure function of
    /// `input`, so that two runs on the same world give the same items. A non-finite position is
    /// dropped; more items than `max_items` end the run with `sq_too_many_items`.
    fn generate(&self, input: &GeneratorInput<'_>) -> Vec<GeneratedItem>;
}

/// What a [`TestImpl`] reads for one (item, anchor value).
pub struct TestInput<'a> {
    /// The world (read only).
    pub world: &'a World,
    /// The entity running the query.
    pub asker: Entity,
    /// The item's position.
    pub position: Vec3,
    /// The item's entity, for an entity item.
    pub entity: Option<Entity>,
    /// The position of the anchor value under test, when the row has a `to` anchor.
    pub anchor: Option<Vec3>,
    /// The entity of the anchor value under test, if it has one.
    pub anchor_entity: Option<Entity>,
    /// The row's `args`, with parameter references resolved.
    pub args: &'a BTreeMap<String, f32>,
    /// Every parameter of the request: declared defaults overridden by the caller's values.
    pub params: &'a BTreeMap<String, ParamValue>,
}

/// What a [`TestImpl`] returns: the raw value the row's filter and score settings read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TestValue {
    /// A boolean, for a row with `value: Bool`.
    Bool(bool),
    /// A float, for a row with `value: Float`. `None` (or a non-finite value) means the test has no value
    /// for this item: it fails a `min` or `max` filter and scores worst.
    Float(Option<f32>),
}

/// A Rust test registered by name on the manager (`Custom(name: "...")` in an asset).
pub trait TestImpl: Send + Sync {
    /// The raw value for one (item, anchor value). Must be a pure function of `input`.
    fn evaluate(&self, input: &TestInput<'_>) -> TestValue;
}

/// The host's tag matcher for `TagMatch`: does `entity` satisfy the tag query string? (`regia_ai`
/// cannot depend on `regia_framework`, ADR-1585.)
pub type TagMatcher = fn(&World, Entity, &str) -> bool;

/// Why a run ended early.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RunError {
    MissingParam(String),
    BadParam(String),
    MissingSystem(String),
    TooManyItems(String),
    NoNavmesh(String),
    ItemTypeMismatch(String),
}

/// A generator or test number that is not finite never reaches the geometry: a NaN spacing
/// silently made a one-point lattice and a NaN radius NaN positions (deep-zai lower note).
/// `what` names the field kind for the message.
fn finite_number(value: f32, what: &str) -> Result<f32, RunError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(RunError::BadParam(format!(
            "{what} number is not finite ({value}); spatial-query math needs finite numbers"
        )))
    }
}

impl RunError {
    fn status(&self) -> QueryStatus {
        match self {
            Self::MissingParam(_) => QueryStatus::MissingParam,
            _ => QueryStatus::Failed,
        }
    }
    fn code(&self) -> Option<&'static str> {
        match self {
            Self::MissingParam(_) => None,
            Self::BadParam(_) => Some("sq_bad_param"),
            Self::MissingSystem(_) => Some("sq_missing_system"),
            Self::TooManyItems(_) => Some("sq_too_many_items"),
            Self::NoNavmesh(_) => Some("sq_no_navmesh"),
            Self::ItemTypeMismatch(_) => Some("sq_item_type_mismatch"),
        }
    }
    fn message(&self) -> String {
        match self {
            Self::MissingParam(name) => {
                format!("parameter `{name}` is used but neither declared nor supplied")
            }
            Self::BadParam(m)
            | Self::MissingSystem(m)
            | Self::TooManyItems(m)
            | Self::NoNavmesh(m)
            | Self::ItemTypeMismatch(m) => m.clone(),
        }
    }
}

/// What the run reads from outside the world.
pub(crate) struct Env<'w> {
    pub world: &'w World,
    pub providers: &'w BTreeMap<String, Box<dyn AnchorProvider>>,
    pub generators: &'w BTreeMap<String, Box<dyn GeneratorImpl>>,
    pub tests: &'w BTreeMap<String, Box<dyn TestImpl>>,
    pub matcher: Option<TagMatcher>,
}

/// The fixed inputs of one run.
pub(crate) struct RunSpec {
    pub id: RequestId,
    pub owner: Entity,
    pub asset: Arc<SpatialQueryAsset>,
    pub overrides: BTreeMap<String, ParamValue>,
    pub pick: Pick,
    pub seed: u64,
    /// The default seed waits for the first tick, which has the world to read the owner's
    /// spawn-order arrival from (a submission has no world; `SpatialQueryManager::submit`).
    pub seed_pending: bool,
    pub keep_failed: bool,
    pub max_items: usize,
    /// Run only the tests with an authoring index up to this one (the probe's `only_through_test`).
    pub through_test: Option<usize>,
    /// Stop with `aborted` past this many units (0 = no limit; the probe's `max_units`).
    pub max_units: u64,
}

#[derive(Debug, Clone, Copy)]
struct AnchorValue {
    pos: Vec3,
    forward: Vec3,
    entity: Option<Entity>,
}

#[derive(Debug, Clone)]
struct Item {
    pos: Vec3,
    entity: Option<Entity>,
    gen_index: usize,
    alive: bool,
    failed_test: Option<usize>,
    failed_reason: Option<&'static str>,
    contributions: Vec<(usize, f32)>,
    num: f32,
    den: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Raw {
    Bool(bool),
    Float(Option<f32>),
}

#[derive(Debug, Clone, Copy)]
enum Pair {
    Unit,
    One(AnchorValue),
    Two(AnchorValue, AnchorValue),
    Dirs(Vec3, Vec3),
}

#[derive(Debug, Clone)]
struct RawItem {
    pos: Vec3,
    entity: Option<Entity>,
    project: bool,
    drop: Option<f32>,
    reach: Option<(Vec3, f32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Filter,
    Score,
}

#[derive(Debug, Clone, Copy)]
struct PlanStep {
    test: usize,
    phase: Phase,
}

/// A zone for `InsideZone`: the inverse of its shape frame, the shape, and its world bounds.
struct Zone {
    inverse: Affine3A,
    shape: ColliderShape,
    bounds: Option<(Vec3, Vec3)>,
}

/// Per-test state resolved once when the test starts.
#[derive(Default)]
struct TestCtx {
    /// Values of anchors that do not read the candidate.
    fixed_to: Option<Vec<AnchorValue>>,
    fixed_from: Option<Vec<AnchorValue>>,
    fixed_dirs: Option<(Vec<Vec3>, Vec<Vec3>)>,
    mask: Option<u32>,
    zones: Vec<Zone>,
    positioned: Vec<(Entity, Vec3)>,
    scan_limit: f32,
    /// The `RouteAll` default `scan_limit` pre-pass, while it has items left to scan.
    scan: Option<ScanCursor>,
    /// A `Custom` test's resolved `args`.
    custom_args: BTreeMap<String, f32>,
}

/// Where the `RouteAll` default `scan_limit` pre-pass stands: one charged unit per alive item, so it
/// pauses and resumes with the tick budget like the evaluations do (spec 33 § 3.6).
#[derive(Default, Clone, Copy)]
struct ScanCursor {
    /// The next item index to scan.
    item: usize,
    /// The largest item-to-anchor distance seen so far.
    largest: f32,
}

#[derive(Default)]
struct Cursor {
    item: usize,
    prepared: bool,
    pairs: Vec<Pair>,
    value: usize,
    partial: Vec<Raw>,
    snap_to: Option<Vec3>,
    ctx: Option<TestCtx>,
    /// Raw values per item for the score pass of the current test.
    raws: HashMap<usize, Vec<Raw>>,
    input: usize,
}

enum Stage {
    Start,
    Generate {
        raw: Vec<RawItem>,
        cursor: usize,
        dedupe: Option<HashMap<(i64, i64, i64), Vec<usize>>>,
    },
    Tests {
        step: usize,
        cursor: Box<Cursor>,
    },
    Finish,
    Done,
}

/// One running request.
pub(crate) struct Run {
    pub spec: RunSpec,
    params: BTreeMap<String, ParamValue>,
    stage: Stage,
    items: Vec<Item>,
    plan: Vec<PlanStep>,
    /// Raw values cached by the filter pass for tests that also score: test → item → values.
    cache: HashMap<usize, HashMap<usize, Vec<Raw>>>,
    summaries: Vec<TestSummary>,
    pub units: u64,
    result: Option<SpatialQueryResult>,
}

/// What one [`Run::step`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// The current stage finished (the run continues).
    StageDone,
    /// The budget ran out mid-stage.
    Paused,
    /// The run finished; take the result.
    Finished,
}

/// The pose of `entity` for a fixed-step query: the local `Transform` chain up its `ChildOf`
/// links, with `GlobalTransform` only for an entity or ancestor that has no `Transform`. The
/// `GlobalTransform` propagates once per frame, so preferring it would make the second fixed step
/// of a frame read the previous step (the frame rate would leak into the result).
fn entity_pose(world: &World, entity: Entity) -> Option<(Vec3, Vec3)> {
    crate::tick_budget::world_pose(world, entity)
        .map(|(position, forward)| (position, forward.normalize_or(Vec3::NEG_Z)))
}

/// The world position of `entity` (its `GlobalTransform`, else its `Transform`).
#[cfg(feature = "spatial-query-probe")]
pub(crate) fn entity_position(world: &World, entity: Entity) -> Option<Vec3> {
    entity_pose(world, entity).map(|(p, _)| p)
}

/// Sorts entities by a spawn-order table: the arrival of `C`, never the entity id. Entities the
/// tracking never saw (no `C` arrival recorded) fall back to the entity id among themselves, at
/// the end of the tracked ones.
fn sort_entities<C: Component>(
    order: Option<&regia_ecs::spawn_order::SpawnOrder<C>>,
    entities: &mut [Entity],
) {
    match order {
        // Arrival order of the component: two runs of the same level that allocated different
        // entity ids still emit items in the same order (ADR-1347).
        Some(order) => order.sort_by_spawn(entities, |e| *e),
        None => entities.sort_unstable_by_key(|e| (u64::MAX, e.index().index(), e.to_bits())),
    }
}

/// Live entities with the scene tag and/or name (both: the entity must match both; neither: every
/// entity with a transform), with a position. Tag sets keep tag-arrival order; a name-only set
/// keeps the `Name` arrival order instead — its entities never had to carry a `Tags`; an
/// unfiltered set keeps the tag-arrival order of `positioned_entities` (untagged entities fall
/// back to the entity id there, per spec 33 § 3.3).
fn entities_matching(world: &World, tag: Option<&str>, name: Option<&str>) -> Vec<Entity> {
    let mut out: Vec<Entity> = match (tag, name) {
        (Some(tag), _) => world
            .try_query::<(Entity, &Tags)>()
            .map(|mut q| {
                q.iter(world)
                    .filter(|(_, t)| t.contains(tag))
                    .map(|(e, _)| e)
                    .collect()
            })
            .unwrap_or_default(),
        (None, Some(name)) => world
            .try_query::<(Entity, &Name)>()
            .map(|mut q| {
                q.iter(world)
                    .filter(|(_, n)| n.as_str() == name)
                    .map(|(e, _)| e)
                    .collect()
            })
            .unwrap_or_default(),
        (None, None) => positioned_entities(world).into_iter().map(|(e, _)| e).collect(),
    };
    if let (Some(_), Some(name)) = (tag, name) {
        out.retain(|e| world.get::<Name>(*e).is_some_and(|n| n.as_str() == name));
    }
    out.retain(|e| entity_pose(world, *e).is_some());
    match (tag, name) {
        (Some(_), _) => sort_entities(
            world.get_resource::<regia_ecs::spawn_order::SpawnOrder<Tags>>(),
            &mut out,
        ),
        (None, Some(_)) => sort_entities(
            world.get_resource::<regia_ecs::spawn_order::SpawnOrder<Name>>(),
            &mut out,
        ),
        (None, None) => {}
    }
    out
}

/// Every entity with a `GlobalTransform` or a `Transform`, in tag-arrival order (entities the
/// tracking never saw fall back to the entity id).
fn positioned_entities(world: &World) -> Vec<(Entity, Vec3)> {
    let order = world.get_resource::<regia_ecs::spawn_order::SpawnOrder<Tags>>();
    let mut entities: Vec<Entity> = Vec::new();
    if let Some(mut q) = world.try_query::<(Entity, &GlobalTransform)>() {
        entities.extend(q.iter(world).map(|(e, _)| e));
    }
    if let Some(mut q) = world.try_query::<(Entity, &Transform)>() {
        entities.extend(q.iter(world).map(|(e, _)| e));
    }
    sort_entities(order, &mut entities);
    entities.dedup();
    entities
        .into_iter()
        .filter_map(|e| entity_pose(world, e).map(|(p, _)| (e, p)))
        .collect()
}

fn planar_angle(v: Vec3) -> Option<f32> {
    let flat = Vec3::new(v.x, 0.0, v.z);
    // `det_atan2`: a platform libm ulp here moves every point a wedge generator emits.
    (flat.length_squared() > 1e-12).then(|| regia_core::math::det_atan2(flat.z, flat.x))
}

/// The direction of a yaw angle on the XZ plane, from the deterministic sin/cos kernel: these
/// offsets are query items a pick then writes into the world, so the bits must not follow the
/// platform libm.
fn yaw_dir(angle: f32) -> Vec3 {
    let (sin, cos) = regia_core::math::det_sin_cos(angle);
    Vec3::new(cos, 0.0, sin)
}

fn quantize(p: Vec3) -> (i64, i64, i64) {
    let q = |v: f32| (f64::from(v) * 1e4).round() as i64;
    (q(p.x), q(p.y), q(p.z))
}

fn layer_mask(world: &World, layers: &[String]) -> Result<Option<u32>, RunError> {
    if layers.is_empty() {
        return Ok(None);
    }
    let Some(table) = world.get_resource::<PhysicsLayers>() else {
        // Without a layer table every layer interacts (the physics crate's rule).
        return Ok(None);
    };
    let mut mask = 0u32;
    for name in layers {
        let index = table
            .index_of(name)
            .ok_or_else(|| RunError::BadParam(format!("unknown physics layer `{name}`")))?;
        mask |= 1u32 << index;
    }
    Ok(Some(mask))
}

fn physics(world: &World) -> Option<WorldQueries<'_>> {
    Some(WorldQueries {
        world: world.get_resource::<PhysicsWorld>()?,
        layers: world.get_resource::<PhysicsLayers>(),
    })
}

fn nav(world: &World) -> Option<regia_nav::NavQuery<'_>> {
    Some(regia_nav::NavQuery {
        mesh: world.get_resource::<regia_nav::NavMesh>()?,
        obstacles: world.get_resource::<regia_nav::NavObstacles>(),
    })
}

fn physics_shape(shape: CastShape) -> Option<ColliderShape> {
    match shape {
        CastShape::Line => None,
        CastShape::Sphere(radius) => Some(ColliderShape::Sphere { radius }),
        CastShape::Box(half_extents) => Some(ColliderShape::Box { half_extents }),
        CastShape::Capsule(radius, half_height) => Some(ColliderShape::Capsule {
            half_height,
            radius,
            axis: ShapeAxis::Y,
        }),
    }
}

fn axis_vector(axis: ShapeAxis) -> Vec3 {
    match axis {
        ShapeAxis::X => Vec3::X,
        ShapeAxis::Y => Vec3::Y,
        ShapeAxis::Z => Vec3::Z,
    }
}

/// The shape's local bounding box, or `None` for an empty or unknown shape.
fn local_bounds(shape: &ColliderShape) -> Option<(Vec3, Vec3)> {
    let along = |axis: ShapeAxis, half: f32, radius: f32| {
        let a = axis_vector(axis);
        let e = a * half + (Vec3::ONE - a) * radius;
        (-e, e)
    };
    let of_points = |points: &[Vec3]| {
        let first = *points.first()?;
        Some(
            points
                .iter()
                .fold((first, first), |(lo, hi), p| (lo.min(*p), hi.max(*p))),
        )
    };
    match shape {
        ColliderShape::Box { half_extents } => Some((-*half_extents, *half_extents)),
        ColliderShape::Sphere { radius } => Some((Vec3::splat(-radius), Vec3::splat(*radius))),
        ColliderShape::Capsule {
            half_height,
            radius,
            axis,
        } => Some(along(*axis, half_height + radius, *radius)),
        ColliderShape::Cylinder {
            half_height,
            radius,
            axis,
        }
        | ColliderShape::Cone {
            half_height,
            radius,
            axis,
        } => Some(along(*axis, *half_height, *radius)),
        ColliderShape::ConvexHull { points } => of_points(points),
        ColliderShape::TriMesh { vertices, .. } => of_points(vertices),
        _ => None,
    }
}

/// Whether the local point `p` is inside the shape (analytic for box, sphere, capsule, cylinder;
/// the local bounds otherwise).
fn contains_local(shape: &ColliderShape, p: Vec3) -> bool {
    match shape {
        ColliderShape::Box { half_extents } => p.abs().cmple(*half_extents).all(),
        ColliderShape::Sphere { radius } => p.length() <= *radius,
        ColliderShape::Capsule {
            half_height,
            radius,
            axis,
        } => {
            if !half_height.is_finite() || *half_height < 0.0 {
                return false;
            }
            let a = axis_vector(*axis);
            let t = p.dot(a).clamp(-half_height, *half_height);
            (p - a * t).length() <= *radius
        }
        ColliderShape::Cylinder {
            half_height,
            radius,
            axis,
        } => {
            let a = axis_vector(*axis);
            let t = p.dot(a);
            t.abs() <= *half_height && (p - a * t).length() <= *radius
        }
        other => local_bounds(other).is_some_and(|(lo, hi)| p.cmpge(lo).all() && p.cmple(hi).all()),
    }
}

impl Run {
    /// A run that has not started.
    pub(crate) fn new(spec: RunSpec) -> Self {
        Self {
            spec,
            params: BTreeMap::new(),
            stage: Stage::Start,
            items: Vec::new(),
            plan: Vec::new(),
            cache: HashMap::new(),
            summaries: Vec::new(),
            units: 0,
            result: None,
        }
    }

    /// The finished result, once [`Step::Finished`] was returned.
    pub(crate) fn take_result(&mut self) -> Option<SpatialQueryResult> {
        self.result.take()
    }

    /// Ends the run with `status` (abort, owner lost, teardown) and returns its result.
    pub(crate) fn end(&mut self, status: QueryStatus, message: Option<String>) -> SpatialQueryResult {
        self.stage = Stage::Done;
        self.result = None;
        SpatialQueryResult {
            status,
            items: Vec::new(),
            tests: self.summaries.clone(),
            item_count: 0,
            units: self.units,
            message,
            code: None,
        }
    }

    fn fail(&mut self, error: RunError) -> Step {
        self.stage = Stage::Done;
        self.result = Some(SpatialQueryResult {
            status: error.status(),
            items: Vec::new(),
            tests: self.summaries.clone(),
            item_count: 0,
            units: self.units,
            message: Some(error.message()),
            code: error.code(),
        });
        Step::Finished
    }

    fn num(&self, n: &Num) -> Result<f32, RunError> {
        match n {
            Num::Lit(v) => finite_number(*v, "a literal"),
            Num::Param(name) => match self.params.get(name) {
                None => Err(RunError::MissingParam(name.clone())),
                Some(value) => value
                    .as_f32()
                    .ok_or_else(|| RunError::BadParam(format!("parameter `{name}` is not a number")))
                    .and_then(|v| finite_number(v, &format!("parameter `{name}`"))),
            },
        }
    }

    fn opt_num(&self, n: Option<&Num>) -> Result<Option<f32>, RunError> {
        n.map(|n| self.num(n)).transpose()
    }

    /// Advances the current stage by at most `budget` units.
    pub(crate) fn step(&mut self, env: &Env<'_>, budget: &mut u64) -> Step {
        if self.spec.max_units > 0 && self.units >= self.spec.max_units && !matches!(self.stage, Stage::Done)
        {
            let result = self.end(
                QueryStatus::Aborted,
                Some(format!("stopped at max_units {}", self.spec.max_units)),
            );
            self.result = Some(result);
            return Step::Finished;
        }
        let stage = std::mem::replace(&mut self.stage, Stage::Done);
        match stage {
            Stage::Start => match self.start(env) {
                Ok(()) => Step::StageDone,
                Err(error) => self.fail(error),
            },
            Stage::Generate {
                raw,
                mut cursor,
                mut dedupe,
            } => {
                let _span = tracing::trace_span!(target: "regia_ai", "spatial_query_generate").entered();
                while cursor < raw.len() {
                    if *budget == 0 || self.units_capped() {
                        self.stage = Stage::Generate { raw, cursor, dedupe };
                        return Step::Paused;
                    }
                    *budget -= 1;
                    self.units += 1;
                    let candidate = &raw[cursor];
                    cursor += 1;
                    if let Some(item) = self.post_process(env, candidate) {
                        if let Some(index) = dedupe.as_mut() {
                            let key = quantize(item.0);
                            let duplicate = (-1..=1).any(|dx| {
                                (-1..=1).any(|dy| {
                                    (-1..=1).any(|dz| {
                                        index
                                            .get(&(
                                                key.0.saturating_add(dx),
                                                key.1.saturating_add(dy),
                                                key.2.saturating_add(dz),
                                            ))
                                            .is_some_and(|list| {
                                                list.iter().any(|&i| {
                                                    #[cfg(test)]
                                                    DEDUPE_COMPARES.with(|cell| cell.set(cell.get() + 1));
                                                    let other = &self.items[i];
                                                    other.entity == item.1
                                                        && (other.pos - item.0).abs().max_element() <= 1e-4
                                                })
                                            })
                                    })
                                })
                            });
                            if duplicate {
                                continue;
                            }
                            index.entry(key).or_default().push(self.items.len());
                        }
                        let gen_index = self.items.len();
                        self.items.push(Item {
                            pos: item.0,
                            entity: item.1,
                            gen_index,
                            alive: true,
                            failed_test: None,
                            failed_reason: None,
                            contributions: Vec::new(),
                            num: 0.0,
                            den: 0.0,
                        });
                    }
                }
                self.stage = Stage::Tests {
                    step: 0,
                    cursor: Box::default(),
                };
                Step::StageDone
            }
            Stage::Tests { step, cursor } => {
                if step >= self.plan.len() {
                    self.stage = Stage::Finish;
                    return Step::StageDone;
                }
                self.step_test(env, budget, step, cursor)
            }
            Stage::Finish => {
                self.finish();
                Step::Finished
            }
            Stage::Done => Step::Finished,
        }
    }

    fn units_capped(&self) -> bool {
        self.spec.max_units > 0 && self.units >= self.spec.max_units
    }

    /// Resolves parameters, checks the backing systems, builds the plan and the raw candidates.
    fn start(&mut self, env: &Env<'_>) -> Result<(), RunError> {
        let asset = Arc::clone(&self.spec.asset);
        // Parameters: declared defaults, then overrides (a type change must stay numeric-to-numeric).
        let mut params = asset.params.clone();
        for (name, value) in &self.spec.overrides {
            if let Some(declared) = params.get(name) {
                let compatible = std::mem::discriminant(declared) == std::mem::discriminant(value)
                    || (declared.as_f32().is_some() && value.as_f32().is_some());
                if !compatible {
                    return Err(RunError::BadParam(format!(
                        "parameter `{name}` is declared {declared:?}, the request supplies {value:?}"
                    )));
                }
            }
            params.insert(name.clone(), value.clone());
        }
        for name in asset.used_params() {
            match params.get(&name) {
                None => return Err(RunError::MissingParam(name)),
                Some(value) if value.as_f32().is_none() => {
                    return Err(RunError::BadParam(format!("parameter `{name}` is not a number")));
                }
                Some(_) => {}
            }
        }
        self.params = params;
        check_systems(&asset, env)?;

        // The plan: snaps, then the other filters (and Route drop_unreachable), then scores.
        let tests: Vec<(usize, &Test)> = asset
            .tests
            .iter()
            .enumerate()
            .filter(|(i, _)| self.spec.through_test.is_none_or(|k| *i <= k))
            .collect();
        let drops_unreachable = |t: &Test| {
            matches!(
                t,
                Test::Route {
                    drop_unreachable: true,
                    ..
                } | Test::RouteAll {
                    drop_unreachable: true,
                    ..
                }
            )
        };
        let mut plan = Vec::new();
        plan.extend(
            tests
                .iter()
                .filter(|(_, t)| matches!(t, Test::Snap { .. }))
                .map(|(i, _)| PlanStep {
                    test: *i,
                    phase: Phase::Filter,
                }),
        );
        plan.extend(
            tests
                .iter()
                .filter(|(_, t)| {
                    !matches!(t, Test::Snap { .. }) && (t.purpose().filters() || drops_unreachable(t))
                })
                .map(|(i, _)| PlanStep {
                    test: *i,
                    phase: Phase::Filter,
                }),
        );
        plan.extend(
            tests
                .iter()
                .filter(|(_, t)| t.purpose().scores())
                .map(|(i, _)| PlanStep {
                    test: *i,
                    phase: Phase::Score,
                }),
        );
        self.plan = plan;
        self.summaries = tests
            .iter()
            .map(|(_, t)| TestSummary {
                id: t.id().to_string(),
                kind: t.kind(),
                input: 0,
                output: 0,
            })
            .collect();

        let mut raw = Vec::new();
        self.generate(env, &asset.generator, asset.item_type, false, &mut raw)?;
        let dedupe = asset.generator.has_union().then(HashMap::new);
        self.stage = Stage::Generate {
            raw,
            cursor: 0,
            dedupe,
        };
        Ok(())
    }

    fn summary_index(&self, test: usize) -> Option<usize> {
        let id = self.spec.asset.tests.get(test)?.id();
        self.summaries.iter().position(|s| s.id == id)
    }

    fn resolve(
        &self,
        env: &Env<'_>,
        anchor: &Anchor,
        candidate: Option<&Item>,
    ) -> Result<Vec<AnchorValue>, RunError> {
        let mut out = Vec::new();
        self.resolve_into(env, anchor, candidate, 0.0, &mut out)?;
        Ok(out)
    }

    fn resolve_into(
        &self,
        env: &Env<'_>,
        anchor: &Anchor,
        candidate: Option<&Item>,
        lift: f32,
        out: &mut Vec<AnchorValue>,
    ) -> Result<(), RunError> {
        let up = |p: Vec3, h: f32| p + Vec3::Y * (h + lift);
        let world = env.world;
        match anchor {
            Anchor::Asker => {
                if let Some((pos, forward)) = entity_pose(world, self.spec.owner) {
                    out.push(AnchorValue {
                        pos: up(pos, 0.0),
                        forward,
                        entity: Some(self.spec.owner),
                    });
                }
            }
            Anchor::Candidate { height } => {
                if let Some(item) = candidate {
                    let forward = item
                        .entity
                        .and_then(|e| entity_pose(world, e))
                        .map_or(Vec3::NEG_Z, |(_, f)| f);
                    out.push(AnchorValue {
                        pos: up(item.pos, *height),
                        forward,
                        entity: item.entity,
                    });
                }
            }
            Anchor::Entity { tag, name, height } => {
                for e in entities_matching(world, tag.as_deref(), name.as_deref()) {
                    if let Some((pos, forward)) = entity_pose(world, e) {
                        out.push(AnchorValue {
                            pos: up(pos, *height),
                            forward,
                            entity: Some(e),
                        });
                    }
                }
            }
            Anchor::Point(p) => out.push(AnchorValue {
                pos: up(*p, 0.0),
                forward: Vec3::NEG_Z,
                entity: None,
            }),
            Anchor::Blackboard(key) => match world
                .get::<Blackboard>(self.spec.owner)
                .and_then(|bb| bb.get(key))
            {
                Some(BbValue::Vec3(p)) => out.push(AnchorValue {
                    pos: up(*p, 0.0),
                    forward: Vec3::NEG_Z,
                    entity: None,
                }),
                Some(BbValue::Entity(bits)) => {
                    if let Some(e) = Entity::try_from_bits(*bits)
                        && world.get_entity(e).is_ok()
                        && let Some((pos, forward)) = entity_pose(world, e)
                    {
                        out.push(AnchorValue {
                            pos: up(pos, 0.0),
                            forward,
                            entity: Some(e),
                        });
                    }
                }
                _ => {}
            },
            Anchor::Provider(name) => {
                let provider = env.providers.get(name).ok_or_else(|| {
                    RunError::MissingSystem(format!("anchor provider `{name}` is not registered"))
                })?;
                let data = provider.provide(world, self.spec.owner);
                out.extend(data.points.iter().map(|p| AnchorValue {
                    pos: up(*p, 0.0),
                    forward: Vec3::NEG_Z,
                    entity: None,
                }));
                for e in data.entities {
                    if let Some((pos, forward)) = entity_pose(world, e) {
                        out.push(AnchorValue {
                            pos: up(pos, 0.0),
                            forward,
                            entity: Some(e),
                        });
                    }
                }
            }
            Anchor::Offset { anchor, height } => {
                self.resolve_into(env, anchor, candidate, lift + height, out)?;
            }
        }
        Ok(())
    }

    fn directions(
        &self,
        env: &Env<'_>,
        d: &Direction,
        candidate: Option<&Item>,
    ) -> Result<Vec<Vec3>, RunError> {
        Ok(match d {
            Direction::Forward(a) => self
                .resolve(env, a, candidate)?
                .iter()
                .map(|v| v.forward)
                .collect(),
            Direction::Between(a, b) => {
                let from = self.resolve(env, a, candidate)?;
                let to = self.resolve(env, b, candidate)?;
                from.iter()
                    .flat_map(|f| to.iter().map(move |t| t.pos - f.pos))
                    .collect()
            }
        })
    }

    fn arc(&self, env: &Env<'_>, arc: Option<&ArcSpec>) -> Result<Option<Option<(f32, f32)>>, RunError> {
        let Some(arc) = arc else {
            return Ok(Some(None));
        };
        let degrees = self.num(&arc.degrees)?.clamp(0.0, 360.0);
        let mid = self
            .directions(env, &arc.toward, None)?
            .first()
            .and_then(|d| planar_angle(*d));
        // An arc whose direction has no value yields nothing.
        Ok(mid.map(|mid| (degrees < 360.0).then_some((mid, degrees.to_radians()))))
    }

    fn post(&self, project: bool, drop: Option<&Num>) -> Result<(bool, Option<f32>), RunError> {
        Ok((project, self.opt_num(drop)?))
    }

    fn check_count(&self, out: &[RawItem]) -> Result<(), RunError> {
        if out.len() > self.spec.max_items {
            return Err(RunError::TooManyItems(format!(
                "the generator produced more than max_items ({}) items",
                self.spec.max_items
            )));
        }
        Ok(())
    }

    fn generate(
        &self,
        env: &Env<'_>,
        g: &Generator,
        declared: ItemType,
        mixed: bool,
        out: &mut Vec<RawItem>,
    ) -> Result<(), RunError> {
        let point = |pos: Vec3, (project, drop): (bool, Option<f32>)| RawItem {
            pos,
            entity: None,
            project,
            drop,
            reach: None,
        };
        match g {
            Generator::NearbyEntities {
                tag,
                name,
                center,
                radius,
            } => {
                let centers = self.resolve(env, center, None)?;
                let radius = self.opt_num(radius.as_ref())?;
                if radius.is_some_and(|r| r <= 0.0) {
                    return Ok(());
                }
                for e in entities_matching(env.world, tag.as_deref(), name.as_deref()) {
                    let Some((pos, _)) = entity_pose(env.world, e) else {
                        continue;
                    };
                    let inside = match radius {
                        None => true,
                        Some(r) => centers.iter().any(|c| (c.pos - pos).length() <= r),
                    };
                    if inside {
                        out.push(RawItem {
                            pos,
                            entity: Some(e),
                            project: false,
                            drop: None,
                            reach: None,
                        });
                        self.check_count(out)?;
                    }
                }
            }
            Generator::AtAnchor {
                anchor,
                project_to_navmesh,
                drop_to_ground,
            } => {
                let post = self.post(*project_to_navmesh, drop_to_ground.as_ref())?;
                for v in self.resolve(env, anchor, None)? {
                    out.push(point(v.pos, post));
                }
                self.check_count(out)?;
            }
            Generator::Ring {
                center,
                radius,
                gap,
                count,
                arc,
                height,
                project_to_navmesh,
                drop_to_ground,
            } => {
                let post = self.post(*project_to_navmesh, drop_to_ground.as_ref())?;
                let r = self.num(radius)?;
                let h = self.num(height)?;
                let Some(arc) = self.arc(env, arc.as_ref())? else {
                    return Ok(());
                };
                if r <= 0.0 {
                    return Ok(());
                }
                let span = arc.map_or(std::f32::consts::TAU, |(_, a)| a);
                let n = if let Some(count) = count {
                    let c = self.num(count)?.round();
                    if c <= 0.0 {
                        return Ok(());
                    }
                    c as usize
                } else {
                    let g = self.opt_num(gap.as_ref())?.unwrap_or(0.0);
                    if g <= 0.0 {
                        return Err(RunError::TooManyItems("Ring gap must be > 0".to_string()));
                    }
                    let along = (r * span / g).floor();
                    let n = if arc.is_some() {
                        along + 1.0
                    } else {
                        along.max(1.0)
                    };
                    if n > self.spec.max_items as f32 {
                        return Err(RunError::TooManyItems(format!("Ring makes {n} points")));
                    }
                    n as usize
                };
                if n > self.spec.max_items {
                    return Err(RunError::TooManyItems(format!("Ring makes {n} points")));
                }
                for c in self.resolve(env, center, None)? {
                    for i in 0..n {
                        let angle = match arc {
                            None => i as f32 * span / n as f32,
                            Some((mid, a)) if n > 1 => mid - a / 2.0 + i as f32 * a / (n - 1) as f32,
                            Some((mid, _)) => mid,
                        };
                        out.push(point(c.pos + yaw_dir(angle) * r + Vec3::Y * h, post));
                    }
                    self.check_count(out)?;
                }
            }
            Generator::Wedge {
                center,
                facing,
                degrees,
                range,
                angle_step,
                aligned_spacing,
                include_center,
                project_to_navmesh,
                drop_to_ground,
            } => {
                let post = self.post(*project_to_navmesh, drop_to_ground.as_ref())?;
                let degrees = self.num(degrees)?.clamp(0.0, 360.0);
                let range = self.num(range)?;
                let step = self.num(angle_step)?.max(1.0);
                let spacing = self.num(aligned_spacing)?;
                if spacing <= 0.0 {
                    return Err(RunError::TooManyItems(
                        "Wedge aligned_spacing must be > 0".to_string(),
                    ));
                }
                let Some(mid) = self
                    .directions(env, facing, None)?
                    .first()
                    .and_then(|d| planar_angle(*d))
                else {
                    return Ok(());
                };
                let rays = (degrees / step + 1e-4).floor() as usize + 1;
                let per_ray = if range > 0.0 {
                    (range / spacing + 1e-4).floor() as usize
                } else {
                    0
                };
                if rays.saturating_mul(per_ray) > self.spec.max_items {
                    return Err(RunError::TooManyItems(format!(
                        "Wedge makes {} points",
                        rays.saturating_mul(per_ray)
                    )));
                }
                for c in self.resolve(env, center, None)? {
                    if *include_center {
                        out.push(point(c.pos, post));
                    }
                    for j in 0..rays {
                        let angle = mid + (-degrees / 2.0 + j as f32 * step).to_radians();
                        let dir = yaw_dir(angle);
                        for i in 1..=per_ray {
                            out.push(point(c.pos + dir * (i as f32 * spacing), post));
                        }
                    }
                    self.check_count(out)?;
                }
            }
            Generator::Annulus {
                center,
                inner_radius,
                outer_radius,
                rings,
                points_per_ring,
                arc,
                spiral,
                project_to_navmesh,
                drop_to_ground,
            } => {
                let post = self.post(*project_to_navmesh, drop_to_ground.as_ref())?;
                let (mut inner, mut outer) = (self.num(inner_radius)?, self.num(outer_radius)?);
                if outer < inner {
                    std::mem::swap(&mut inner, &mut outer);
                }
                let rings = self.num(rings)?.round();
                let per = self.num(points_per_ring)?.round();
                let spiral = self.opt_num(spiral.as_ref())?.unwrap_or(0.0).to_radians();
                let Some(arc) = self.arc(env, arc.as_ref())? else {
                    return Ok(());
                };
                if rings <= 0.0 || per <= 0.0 {
                    return Ok(());
                }
                if rings * per > self.spec.max_items as f32 {
                    return Err(RunError::TooManyItems(format!(
                        "Annulus makes {} points",
                        rings * per
                    )));
                }
                let (rings, per) = (rings as usize, per as usize);
                for c in self.resolve(env, center, None)? {
                    for i in 0..rings {
                        let r = if rings == 1 {
                            (inner + outer) / 2.0
                        } else {
                            inner + i as f32 * (outer - inner) / (rings - 1) as f32
                        };
                        let turn = i as f32 * spiral;
                        for j in 0..per {
                            let angle = match arc {
                                None => j as f32 * std::f32::consts::TAU / per as f32,
                                Some((mid, a)) if per > 1 => mid - a / 2.0 + j as f32 * a / (per - 1) as f32,
                                Some((mid, _)) => mid,
                            } + turn;
                            out.push(point(c.pos + yaw_dir(angle) * r, post));
                        }
                    }
                    self.check_count(out)?;
                }
            }
            Generator::Lattice {
                center,
                half_size,
                spacing,
                project_to_navmesh,
                drop_to_ground,
            } => {
                let post = self.post(*project_to_navmesh, drop_to_ground.as_ref())?;
                let half = self.num(half_size)?;
                let spacing = self.num(spacing)?;
                for c in self.resolve(env, center, None)? {
                    for p in self.lattice(c.pos, half, spacing)? {
                        out.push(point(p, post));
                    }
                    self.check_count(out)?;
                }
            }
            Generator::ReachLattice {
                center,
                half_size,
                spacing,
                reach_limit,
                drop_to_ground,
            } => {
                let drop = self.opt_num(drop_to_ground.as_ref())?;
                let half = self.num(half_size)?;
                let spacing = self.num(spacing)?;
                let limit = self.num(reach_limit)? * half;
                for c in self.resolve(env, center, None)? {
                    for p in self.lattice(c.pos, half, spacing)? {
                        out.push(RawItem {
                            pos: p,
                            entity: None,
                            project: true,
                            drop,
                            reach: Some((c.pos, limit)),
                        });
                    }
                    self.check_count(out)?;
                }
            }
            Generator::Custom {
                name,
                center,
                args,
                project_to_navmesh,
                drop_to_ground,
            } => {
                let post = self.post(*project_to_navmesh, drop_to_ground.as_ref())?;
                let implementation = env.generators.get(name).ok_or_else(|| {
                    RunError::MissingSystem(format!("custom generator `{name}` is not registered"))
                })?;
                let centers: Vec<Vec3> = self.resolve(env, center, None)?.iter().map(|c| c.pos).collect();
                let args = self.custom_args(args)?;
                let items = implementation.generate(&GeneratorInput {
                    world: env.world,
                    asker: self.spec.owner,
                    centers: &centers,
                    args: &args,
                    params: &self.params,
                    seed: self.spec.seed,
                });
                for item in items {
                    match item {
                        GeneratedItem::Point(pos) => {
                            if declared == ItemType::Entity {
                                return Err(RunError::ItemTypeMismatch(format!(
                                    "custom generator `{name}` produced a point item in an entity query"
                                )));
                            }
                            if pos.is_finite() {
                                out.push(point(pos, post));
                            }
                        }
                        GeneratedItem::Entity(entity) => {
                            if declared == ItemType::Point && !mixed {
                                return Err(RunError::ItemTypeMismatch(format!(
                                    "custom generator `{name}` produced an entity item in a point query"
                                )));
                            }
                            if let Some((pos, _)) = entity_pose(env.world, entity) {
                                out.push(RawItem {
                                    pos,
                                    entity: Some(entity),
                                    project: false,
                                    drop: None,
                                    reach: None,
                                });
                            }
                        }
                    }
                }
                self.check_count(out)?;
            }
            Generator::Union {
                children,
                allow_mixed,
            } => {
                for child in children {
                    let start = out.len();
                    self.generate(env, child, declared, *allow_mixed, out)?;
                    if declared == ItemType::Point {
                        // allow_mixed (checked at load): entity items become positions.
                        for item in &mut out[start..] {
                            item.entity = None;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The `args` of a custom row with parameter references resolved.
    fn custom_args(&self, args: &BTreeMap<String, Num>) -> Result<BTreeMap<String, f32>, RunError> {
        args.iter()
            .map(|(name, n)| Ok((name.clone(), self.num(n)?)))
            .collect()
    }

    fn lattice(&self, center: Vec3, half: f32, spacing: f32) -> Result<Vec<Vec3>, RunError> {
        if spacing <= 0.0 {
            return Err(RunError::TooManyItems("Lattice spacing must be > 0".to_string()));
        }
        if half < 0.0 {
            return Ok(Vec::new());
        }
        let k = (half / spacing + 1e-4).floor();
        let side = 2.0 * k + 1.0;
        if side * side > self.spec.max_items as f32 {
            return Err(RunError::TooManyItems(format!(
                "the lattice has {} points; max_items is {}",
                side * side,
                self.spec.max_items
            )));
        }
        let k = k as i64;
        let mut out = Vec::with_capacity((side * side) as usize);
        for iz in -k..=k {
            for ix in -k..=k {
                out.push(center + Vec3::new(ix as f32 * spacing, 0.0, iz as f32 * spacing));
            }
        }
        Ok(out)
    }

    /// Projection, reach check and ground trace of one candidate. `None` drops it.
    fn post_process(&self, env: &Env<'_>, raw: &RawItem) -> Option<(Vec3, Option<Entity>)> {
        let mut pos = raw.pos;
        if raw.project {
            let query = nav(env.world)?;
            pos = query.find_nearest(pos, self.spec.asset.extents)?.1;
        }
        if let Some((center, limit)) = raw.reach {
            let query = nav(env.world)?;
            let path = query.find_path(center, pos, self.spec.asset.extents)?;
            if path.length > limit {
                return None;
            }
        }
        if let Some(h) = raw.drop
            && h > 0.0
        {
            let queries = physics(env.world)?;
            let exclude = [self.spec.owner];
            let hit = queries.raycast(
                pos + Vec3::Y * h,
                Vec3::NEG_Y,
                2.0 * h,
                QueryFilter::new().exclude_entities(&exclude),
            )?;
            pos.y = hit.point.y;
        }
        Some((pos, raw.entity))
    }

    fn prepare_ctx(&self, env: &Env<'_>, test: &Test) -> Result<TestCtx, RunError> {
        let mut ctx = TestCtx::default();
        let fixed = |a: &Anchor| -> Result<Option<Vec<AnchorValue>>, RunError> {
            if a.uses_candidate() {
                Ok(None)
            } else {
                self.resolve(env, a, None).map(Some)
            }
        };
        match test {
            Test::Distance { to, .. } | Test::Route { to, .. } => ctx.fixed_to = fixed(to)?,
            Test::RouteAll { to, scan_limit, .. } => {
                ctx.fixed_to = fixed(to)?;
                match self.opt_num(scan_limit.as_ref())? {
                    Some(limit) => ctx.scan_limit = limit,
                    // The default is 1.5 times the largest item-to-anchor distance, found by a
                    // budgeted pre-pass in `step_test`.
                    None => ctx.scan = Some(ScanCursor::default()),
                }
            }
            Test::Cast { from, to, layers, .. } => {
                ctx.fixed_from = fixed(from)?;
                ctx.fixed_to = fixed(to)?;
                ctx.mask = layer_mask(env.world, layers)?;
            }
            Test::Clearance { layers, .. } => ctx.mask = layer_mask(env.world, layers)?,
            Test::Snap {
                target: SnapTarget::Geometry(layers),
                ..
            } => ctx.mask = layer_mask(env.world, layers)?,
            Test::Facing { a, b, .. } => {
                if !a.uses_candidate() && !b.uses_candidate() {
                    ctx.fixed_dirs = Some((self.directions(env, a, None)?, self.directions(env, b, None)?));
                }
            }
            Test::InsideZone { tag, .. } => {
                for e in entities_matching(env.world, Some(tag), None) {
                    let Some(collider) = env.world.get::<Collider>(e) else {
                        continue;
                    };
                    // The zone's frame comes from the same local-chain walk the query poses use
                    // (ADR-4651 (2)): the `GlobalTransform` snapshot is a frame old on a later
                    // fixed step, which would test against the previous frame's zone.
                    let base = crate::tick_budget::affine_from_chain(
                        env.world.get::<Transform>(e),
                        env.world.get::<GlobalTransform>(e),
                        env.world.get::<ChildOf>(e),
                        |ent| {
                            (
                                env.world.get::<Transform>(ent),
                                env.world.get::<GlobalTransform>(ent),
                                env.world.get::<ChildOf>(ent),
                            )
                        },
                    );
                    let Some(base) = base else {
                        continue;
                    };
                    let frame = base * collider.offset.compute_affine();
                    let bounds = local_bounds(&collider.shape).map(|(lo, hi)| {
                        let corners = (0..8).map(|i| {
                            frame.transform_point3(Vec3::new(
                                if i & 1 == 0 { lo.x } else { hi.x },
                                if i & 2 == 0 { lo.y } else { hi.y },
                                if i & 4 == 0 { lo.z } else { hi.z },
                            ))
                        });
                        corners.fold((Vec3::INFINITY, Vec3::NEG_INFINITY), |(a, b), p| {
                            (a.min(p), b.max(p))
                        })
                    });
                    ctx.zones.push(Zone {
                        inverse: frame.inverse(),
                        shape: collider.shape.clone(),
                        bounds,
                    });
                }
            }
            Test::TagMatch { .. } => {
                if self.spec.asset.item_type == ItemType::Point {
                    ctx.positioned = positioned_entities(env.world);
                }
            }
            Test::Snap { .. } => {}
            Test::Custom { to, args, .. } => {
                if let Some(to) = to {
                    ctx.fixed_to = fixed(to)?;
                }
                ctx.custom_args = self.custom_args(args)?;
            }
        }
        Ok(ctx)
    }

    /// Appends one anchor's values. A fixed anchor is copied; resolving a moving anchor still
    /// allocates its own list.
    fn push_anchor(
        &self,
        env: &Env<'_>,
        fixed: &Option<Vec<AnchorValue>>,
        anchor: &Anchor,
        item: &Item,
        out: &mut Vec<AnchorValue>,
    ) -> Result<(), RunError> {
        out.clear();
        if let Some(values) = fixed {
            out.extend_from_slice(values);
            Ok(())
        } else {
            self.resolve_into(env, anchor, Some(item), 0.0, out)
        }
    }

    /// Fills `out` with the pairs of one item. The caller keeps `out` across items, so a fixed
    /// anchor (the common distance-to-asker filter) does not allocate after the first item.
    fn fill_pairs(
        &self,
        env: &Env<'_>,
        test: &Test,
        ctx: &TestCtx,
        item: &Item,
        out: &mut Vec<Pair>,
    ) -> Result<(), RunError> {
        out.clear();
        match test {
            Test::Distance { to, .. } | Test::Route { to, .. } | Test::RouteAll { to, .. } => {
                if let Some(values) = &ctx.fixed_to {
                    out.extend(values.iter().copied().map(Pair::One));
                } else {
                    let resolved = self.resolve(env, to, Some(item))?;
                    out.extend(resolved.into_iter().map(Pair::One));
                }
            }
            Test::Cast { from, to, .. } => {
                let mut from_values = Vec::new();
                let mut to_values = Vec::new();
                self.push_anchor(env, &ctx.fixed_from, from, item, &mut from_values)?;
                self.push_anchor(env, &ctx.fixed_to, to, item, &mut to_values)?;
                for from_value in &from_values {
                    for to_value in &to_values {
                        out.push(Pair::Two(*from_value, *to_value));
                    }
                }
            }
            Test::Facing { a, b, .. } => match &ctx.fixed_dirs {
                Some((da, db)) => {
                    for x in da {
                        for y in db {
                            out.push(Pair::Dirs(*x, *y));
                        }
                    }
                }
                None => {
                    let da = self.directions(env, a, Some(item))?;
                    let db = self.directions(env, b, Some(item))?;
                    for x in &da {
                        for y in &db {
                            out.push(Pair::Dirs(*x, *y));
                        }
                    }
                }
            },
            Test::InsideZone { .. } | Test::TagMatch { .. } | Test::Clearance { .. } | Test::Snap { .. } => {
                out.push(Pair::Unit);
            }
            Test::Custom { to: Some(to), .. } => {
                if let Some(values) = &ctx.fixed_to {
                    out.extend(values.iter().copied().map(Pair::One));
                } else {
                    let resolved = self.resolve(env, to, Some(item))?;
                    out.extend(resolved.into_iter().map(Pair::One));
                }
            }
            Test::Custom { to: None, .. } => out.push(Pair::Unit),
        }
        Ok(())
    }

    /// Evaluates one (item, anchor value) of a `Custom` test.
    fn evaluate_custom(
        &self,
        env: &Env<'_>,
        test: &Test,
        ctx: &TestCtx,
        item: &Item,
        pair: Pair,
    ) -> Result<Raw, RunError> {
        let Test::Custom { name, value, .. } = test else {
            return Ok(Raw::Bool(false));
        };
        let implementation = env
            .tests
            .get(name)
            .ok_or_else(|| RunError::MissingSystem(format!("custom test `{name}` is not registered")))?;
        let (anchor, anchor_entity) = match pair {
            Pair::One(v) => (Some(v.pos), v.entity),
            _ => (None, None),
        };
        let result = implementation.evaluate(&TestInput {
            world: env.world,
            asker: self.spec.owner,
            position: item.pos,
            entity: item.entity,
            anchor,
            anchor_entity,
            args: &ctx.custom_args,
            params: &self.params,
        });
        match (result, value) {
            (TestValue::Bool(b), CustomValue::Bool) => Ok(Raw::Bool(b)),
            (TestValue::Float(v), CustomValue::Float) => Ok(Raw::Float(v.filter(|v| v.is_finite()))),
            (_, declared) => Err(RunError::BadParam(format!(
                "custom test `{name}` returned {result:?}, the query declares {declared:?}"
            ))),
        }
    }

    /// Evaluates one (item, test, anchor value): the raw value, and for `Snap` the new position.
    fn evaluate(
        &self,
        env: &Env<'_>,
        test: &Test,
        ctx: &TestCtx,
        item: &Item,
        pair: Pair,
    ) -> (Raw, Option<Vec3>) {
        let world = env.world;
        let p = item.pos;
        match (test, pair) {
            (Test::Distance { mode, .. }, Pair::One(v)) => {
                let d = v.pos - p;
                let raw = match mode {
                    DistanceMode::Full3d => d.length(),
                    DistanceMode::Planar => Vec3::new(d.x, 0.0, d.z).length(),
                    DistanceMode::Vertical => p.y - v.pos.y,
                    DistanceMode::AbsVertical => (p.y - v.pos.y).abs(),
                };
                (Raw::Float(Some(raw)), None)
            }
            (Test::Facing { planar, abs, .. }, Pair::Dirs(a, b)) => {
                let flat = |v: Vec3| if *planar { Vec3::new(v.x, 0.0, v.z) } else { v };
                let dot = flat(a).normalize_or_zero().dot(flat(b).normalize_or_zero());
                (Raw::Float(Some(if *abs { dot.abs() } else { dot })), None)
            }
            (Test::InsideZone { mode, .. }, _) => {
                let inside = ctx.zones.iter().any(|zone| match mode {
                    ZoneMode::Exact => contains_local(&zone.shape, zone.inverse.transform_point3(p)),
                    ZoneMode::Bounds => zone
                        .bounds
                        .is_some_and(|(lo, hi)| p.cmpge(lo).all() && p.cmple(hi).all()),
                });
                (Raw::Bool(inside), None)
            }
            (Test::TagMatch { query, .. }, _) => {
                let target = item.entity.or_else(|| {
                    ctx.positioned
                        .iter()
                        .map(|(e, q)| (*e, (*q - p).length_squared()))
                        .fold(None, |best: Option<(Entity, f32)>, (e, d)| match best {
                            Some((_, bd)) if bd <= d => best,
                            _ => Some((e, d)),
                        })
                        .map(|(e, _)| e)
                });
                let matched = match (target, env.matcher) {
                    (Some(e), Some(matcher)) => matcher(world, e, query),
                    _ => false,
                };
                (Raw::Bool(matched), None)
            }
            (
                Test::Cast {
                    shape, blocking_only, ..
                },
                Pair::Two(from, to),
            ) => {
                let Some(queries) = physics(world) else {
                    return (Raw::Bool(false), None);
                };
                let mut exclude = vec![self.spec.owner];
                exclude.extend(item.entity);
                exclude.extend(from.entity);
                exclude.extend(to.entity);
                let mut filter = QueryFilter::new().exclude_entities(&exclude);
                if let Some(mask) = ctx.mask {
                    filter = filter.layers(mask);
                }
                if *blocking_only {
                    filter = filter.exclude_sensors();
                }
                let delta = to.pos - from.pos;
                let distance = delta.length();
                let hit = if distance <= 1e-6 {
                    false
                } else {
                    match physics_shape(*shape) {
                        None => queries.raycast(from.pos, delta, distance, filter).is_some(),
                        Some(volume) => queries
                            .shapecast(
                                &volume,
                                Transform::from_translation(from.pos),
                                delta / distance,
                                distance,
                                filter,
                            )
                            .is_some(),
                    }
                };
                (Raw::Bool(hit), None)
            }
            (
                Test::Clearance {
                    shape,
                    offset,
                    blocking_only,
                    skip_asker,
                    ..
                },
                _,
            ) => {
                let Some(queries) = physics(world) else {
                    return (Raw::Bool(false), None);
                };
                let mut exclude: Vec<Entity> = item.entity.into_iter().collect();
                if *skip_asker {
                    exclude.push(self.spec.owner);
                }
                let mut filter = QueryFilter::new().exclude_entities(&exclude);
                if let Some(mask) = ctx.mask {
                    filter = filter.layers(mask);
                }
                if *blocking_only {
                    filter = filter.exclude_sensors();
                }
                let overlaps = physics_shape(*shape).is_some_and(|volume| {
                    !queries
                        .overlap(&volume, Transform::from_translation(p + *offset), filter)
                        .is_empty()
                });
                (Raw::Bool(overlaps), None)
            }
            (
                Test::Route {
                    direction, measure, ..
                }
                | Test::RouteAll {
                    direction, measure, ..
                },
                Pair::One(v),
            ) => {
                let skip = matches!(test, Test::RouteAll { .. }) && (v.pos - p).length() > ctx.scan_limit;
                let length = if skip {
                    None
                } else {
                    nav(world).and_then(|query| {
                        let (a, b) = match direction {
                            RouteDirection::ToAnchor => (p, v.pos),
                            RouteDirection::FromAnchor => (v.pos, p),
                        };
                        query
                            .find_path(a, b, self.spec.asset.extents)
                            .map(|path| path.length)
                    })
                };
                match measure {
                    RouteMeasure::Exists => (Raw::Bool(length.is_some()), None),
                    RouteMeasure::Length => (Raw::Float(length), None),
                }
            }
            (
                Test::Snap {
                    target,
                    up,
                    down,
                    offset,
                    ..
                },
                _,
            ) => {
                let up = self.num(up).unwrap_or(0.0).max(0.0);
                let down = self.num(down).unwrap_or(0.0).max(0.0);
                let snapped = match target {
                    SnapTarget::NavMesh => nav(world).and_then(|query| {
                        let center = p + Vec3::Y * ((up - down) / 2.0);
                        let extents = self.spec.asset.extents;
                        query
                            .find_nearest(center, Vec3::new(extents.x, (up + down) / 2.0, extents.z))
                            .map(|(_, q)| q)
                    }),
                    SnapTarget::Geometry(_) => physics(world).and_then(|queries| {
                        let exclude: Vec<Entity> = item.entity.into_iter().chain([self.spec.owner]).collect();
                        let mut filter = QueryFilter::new().exclude_entities(&exclude);
                        if let Some(mask) = ctx.mask {
                            filter = filter.layers(mask);
                        }
                        queries
                            .raycast(p + Vec3::Y * up, Vec3::NEG_Y, up + down, filter)
                            .map(|hit| hit.point)
                    }),
                };
                (Raw::Bool(snapped.is_some()), snapped.map(|q| q + *offset))
            }
            // A pair that does not fit the test (unreachable by construction).
            _ => (Raw::Bool(false), None),
        }
    }

    /// Runs the `RouteAll` default `scan_limit` pre-pass of `ctx` (if it has one) within `budget`, one
    /// unit per alive item. `Ok(false)` when the budget ran out first; the scan resumes next time.
    fn scan_route_all(
        &mut self,
        env: &Env<'_>,
        to: &Anchor,
        ctx: &mut TestCtx,
        budget: &mut u64,
    ) -> Result<bool, RunError> {
        let Some(mut scan) = ctx.scan.take() else {
            return Ok(true);
        };
        while scan.item < self.items.len() {
            let item = &self.items[scan.item];
            if !item.alive {
                scan.item += 1;
                continue;
            }
            if *budget == 0 || self.units_capped() {
                ctx.scan = Some(scan);
                return Ok(false);
            }
            let origin = item.pos;
            let resolved;
            let values = match &ctx.fixed_to {
                Some(values) => values.as_slice(),
                None => {
                    #[cfg(test)]
                    SCAN_RESOLVES.with(|cell| cell.set(cell.get() + 1));
                    resolved = self.resolve(env, to, Some(item))?;
                    resolved.as_slice()
                }
            };
            for v in values {
                scan.largest = scan.largest.max((v.pos - origin).length());
            }
            *budget -= 1;
            self.units += 1;
            scan.item += 1;
        }
        ctx.scan_limit = 1.5 * scan.largest;
        Ok(true)
    }

    fn step_test(&mut self, env: &Env<'_>, budget: &mut u64, step: usize, mut cursor: Box<Cursor>) -> Step {
        let PlanStep {
            test: test_index,
            phase,
        } = self.plan[step];
        let asset = Arc::clone(&self.spec.asset);
        let test = &asset.tests[test_index];
        let _span = tracing::trace_span!(target: "regia_ai", "spatial_query_test", id = test.id()).entered();

        // A score pass of a test the filter pass already evaluated reuses its values.
        let cached = phase == Phase::Score && self.cache.contains_key(&test_index);
        if cursor.ctx.is_none() && !cached {
            match self.prepare_ctx(env, test) {
                Ok(ctx) => cursor.ctx = Some(ctx),
                Err(error) => return self.fail(error),
            }
            cursor.input = self.items.iter().filter(|i| i.alive).count();
        }
        if !cached
            && let Test::RouteAll { to, .. } = test
            && let Some(mut ctx) = cursor.ctx.take()
        {
            let scanned = self.scan_route_all(env, to, &mut ctx, budget);
            cursor.ctx = Some(ctx);
            match scanned {
                Ok(true) => {}
                Ok(false) => {
                    self.stage = Stage::Tests { step, cursor };
                    return Step::Paused;
                }
                Err(error) => return self.fail(error),
            }
        }
        if !cached {
            while cursor.item < self.items.len() {
                if !self.items[cursor.item].alive {
                    cursor.item += 1;
                    continue;
                }
                if !cursor.prepared {
                    // `ctx` and `pairs` are the same cursor, so take the context out while filling.
                    let ctx = cursor.ctx.take();
                    let filled = match ctx.as_ref() {
                        Some(ctx) => {
                            self.fill_pairs(env, test, ctx, &self.items[cursor.item], &mut cursor.pairs)
                        }
                        None => {
                            cursor.pairs.clear();
                            Ok(())
                        }
                    };
                    cursor.ctx = ctx;
                    if let Err(error) = filled {
                        return self.fail(error);
                    }
                    cursor.prepared = true;
                    cursor.value = 0;
                    cursor.partial.clear();
                    cursor.snap_to = None;
                }
                while cursor.value < cursor.pairs.len() {
                    if *budget == 0 || self.units_capped() {
                        self.stage = Stage::Tests { step, cursor };
                        return Step::Paused;
                    }
                    *budget -= 1;
                    self.units += 1;
                    let pair = cursor.pairs[cursor.value];
                    let Some(ctx) = cursor.ctx.as_ref() else {
                        break;
                    };
                    let (raw, snap) = if matches!(test, Test::Custom { .. }) {
                        match self.evaluate_custom(env, test, ctx, &self.items[cursor.item], pair) {
                            Ok(raw) => (raw, None),
                            Err(error) => return self.fail(error),
                        }
                    } else {
                        self.evaluate(env, test, ctx, &self.items[cursor.item], pair)
                    };
                    cursor.partial.push(raw);
                    cursor.snap_to = snap.or(cursor.snap_to);
                    cursor.value += 1;
                }
                let item_index = cursor.item;
                match phase {
                    Phase::Filter => {
                        if let Err(error) =
                            self.filter_item(test_index, test, item_index, &cursor.partial, cursor.snap_to)
                        {
                            return self.fail(error);
                        }
                        // A filter that also scores has to keep the values. A filter-only test
                        // clears on the next item and keeps the buffer.
                        if test.purpose().scores() {
                            let raws = std::mem::take(&mut cursor.partial);
                            self.cache.entry(test_index).or_default().insert(item_index, raws);
                        }
                    }
                    Phase::Score => {
                        let raws = std::mem::take(&mut cursor.partial);
                        cursor.raws.insert(item_index, raws);
                    }
                }
                cursor.item += 1;
                cursor.prepared = false;
            }
        }
        let survivors = self.items.iter().filter(|i| i.alive).count();
        if let Some(index) = self.summary_index(test_index) {
            let summary = &mut self.summaries[index];
            match phase {
                Phase::Filter => {
                    summary.input = cursor.input;
                    summary.output = survivors;
                }
                Phase::Score if !test.purpose().filters() && !cached => {
                    summary.input = cursor.input;
                    summary.output = survivors;
                }
                Phase::Score => {}
            }
        }
        if phase == Phase::Score {
            let raws = if cached {
                self.cache.remove(&test_index).unwrap_or_default()
            } else {
                std::mem::take(&mut cursor.raws)
            };
            if let Err(error) = self.score_test(test_index, test, &raws) {
                return self.fail(error);
            }
        }
        self.stage = Stage::Tests {
            step: step + 1,
            cursor: Box::default(),
        };
        Step::StageDone
    }

    fn filter_item(
        &mut self,
        test_index: usize,
        test: &Test,
        item_index: usize,
        raws: &[Raw],
        snap_to: Option<Vec3>,
    ) -> Result<(), RunError> {
        let common = test.common();
        let (fails, reason) = if let Test::Snap { .. } = test {
            match snap_to {
                Some(p) => {
                    self.items[item_index].pos = p;
                    (false, "")
                }
                None => (true, "snap"),
            }
        } else if raws.is_empty() {
            (true, "no_anchor")
        } else {
            let filters = test.purpose().filters();
            let min = if filters { self.opt_num(common.min)? } else { None };
            let max = if filters { self.opt_num(common.max)? } else { None };
            let drop_unreachable = matches!(
                test,
                Test::Route {
                    drop_unreachable: true,
                    ..
                } | Test::RouteAll {
                    drop_unreachable: true,
                    ..
                }
            );
            let want = bool_want(test);
            let pass = |raw: &Raw| match raw {
                Raw::Bool(b) => !filters || *b == want,
                Raw::Float(None) => !drop_unreachable && min.is_none() && max.is_none(),
                Raw::Float(Some(v)) => min.is_none_or(|m| *v >= m) && max.is_none_or(|m| *v <= m),
            };
            let ok = match common.require {
                Require::Every => raws.iter().all(pass),
                Require::Some => raws.iter().any(pass),
            };
            (
                !ok,
                if drop_unreachable && raws.iter().all(|r| *r == Raw::Float(None)) {
                    "unreachable"
                } else {
                    "filter"
                },
            )
        };
        if fails {
            let item = &mut self.items[item_index];
            item.alive = false;
            item.failed_test = Some(test_index);
            item.failed_reason = Some(reason);
        }
        Ok(())
    }

    fn score_test(
        &mut self,
        test_index: usize,
        test: &Test,
        raws: &HashMap<usize, Vec<Raw>>,
    ) -> Result<(), RunError> {
        let common = test.common();
        let Some(spec) = common.score else {
            return Ok(());
        };
        let weight = self.num(&spec.weight)?;
        let alive: Vec<usize> = (0..self.items.len()).filter(|i| self.items[*i].alive).collect();
        let curve = if test.is_float() {
            let seen = alive
                .iter()
                .filter_map(|i| raws.get(i))
                .flatten()
                .filter_map(|r| match r {
                    Raw::Float(Some(v)) => Some(*v),
                    _ => None,
                })
                .fold(None, |acc: Option<(f32, f32)>, v| match acc {
                    None => Some((v, v)),
                    Some((lo, hi)) => Some((lo.min(v), hi.max(v))),
                })
                .unwrap_or((0.0, 0.0));
            let bound = |clamp: &Clamp, filter: Option<&Num>, seen: f32| -> Result<f32, RunError> {
                Ok(match clamp {
                    Clamp::Fixed(n) => self.num(n)?,
                    Clamp::FilterBound => self.opt_num(filter)?.unwrap_or(seen),
                    Clamp::Seen => seen,
                })
            };
            Some(ScoreCurve {
                lo: bound(&spec.clamp_min, common.min, seen.0)?,
                hi: bound(&spec.clamp_max, common.max, seen.1)?,
                origin: spec.origin,
                peak_at: self.opt_num(spec.peak_at.as_ref())?,
                shape: spec.shape,
            })
        } else {
            None
        };
        let want = bool_want(test);
        for i in alive {
            let Some(values) = raws.get(&i) else {
                continue;
            };
            let shaped: Vec<f32> = values
                .iter()
                .map(|r| match (r, &curve) {
                    (Raw::Bool(b), _) => {
                        if *b == want {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    (Raw::Float(Some(v)), Some(c)) => c.shaped(*v),
                    (Raw::Float(None), Some(c)) => c.worst(weight),
                    (Raw::Float(_), None) => 0.0,
                })
                .collect();
            // No anchor value: the test contributes nothing (not zero).
            let Some(s) = combine(common.combine, &shaped) else {
                continue;
            };
            let contribution = s * weight;
            let item = &mut self.items[i];
            item.num += contribution;
            item.den += weight.abs();
            item.contributions.push((test_index, contribution));
        }
        Ok(())
    }

    fn finish(&mut self) {
        let asset = Arc::clone(&self.spec.asset);
        let score = |item: &Item| if item.den > 0.0 { item.num / item.den } else { 0.0 };
        let mut survivors: Vec<&Item> = self.items.iter().filter(|i| i.alive).collect();
        survivors.sort_by(|a, b| score(b).total_cmp(&score(a)).then(a.gen_index.cmp(&b.gen_index)));
        let item_count = survivors.len();
        let to_scored = |item: &Item, passed: bool| ScoredItem {
            position: item.pos,
            entity: item.entity,
            score: score(item),
            passed,
            failed_test: item
                .failed_test
                .and_then(|t| asset.tests.get(t))
                .map(|t| t.id().to_string()),
            failed_reason: item.failed_reason.map(str::to_string),
            contributions: item
                .contributions
                .iter()
                .filter_map(|(t, c)| asset.tests.get(*t).map(|t| (t.id().to_string(), *c)))
                .collect(),
            generation_index: item.gen_index,
        };
        let picked: Vec<&Item> = match self.spec.pick {
            Pick::All => survivors.clone(),
            Pick::Best => survivors.first().copied().into_iter().collect(),
            Pick::Sample { band } => match (survivors.first(), survivors.last()) {
                (Some(best), Some(worst)) => {
                    let (best, worst) = (score(best), score(worst));
                    let threshold = best - band.clamp(0.0, 1.0) * (best - worst);
                    let in_band = survivors
                        .iter()
                        .take_while(|i| score(i) >= threshold)
                        .count()
                        .max(1);
                    let mut rng = Rng::new(self.spec.seed);
                    let pick = rng.range(0u32..in_band as u32) as usize;
                    vec![survivors[pick]]
                }
                _ => Vec::new(),
            },
        };
        let status = if picked.is_empty() && !matches!(self.spec.pick, Pick::All) {
            QueryStatus::Failed
        } else {
            QueryStatus::Success
        };
        let mut items: Vec<ScoredItem> = picked.into_iter().map(|i| to_scored(i, true)).collect();
        if self.spec.keep_failed {
            items.extend(
                self.items
                    .iter()
                    .filter(|i| !i.alive)
                    .map(|i| to_scored(i, false)),
            );
        }
        self.stage = Stage::Done;
        self.result = Some(SpatialQueryResult {
            status,
            items,
            tests: self.summaries.clone(),
            item_count,
            units: self.units,
            message: (status == QueryStatus::Failed).then(|| "no item survived the filters".to_string()),
            code: None,
        });
    }
}

fn bool_want(test: &Test) -> bool {
    match test {
        Test::Cast { want_hit, .. } | Test::Clearance { want_hit, .. } => *want_hit,
        Test::InsideZone { want, .. } | Test::TagMatch { want, .. } | Test::Custom { want, .. } => *want,
        _ => true,
    }
}

/// Fails the run up front when a backing system is absent (§ 3.4): `sq_no_navmesh` for a
/// `ReachLattice` without a navigation mesh, `sq_missing_system` naming the system otherwise.
fn check_systems(asset: &SpatialQueryAsset, env: &Env<'_>) -> Result<(), RunError> {
    fn walk(g: &Generator, nav: &mut bool, reach: &mut bool, physics: &mut bool) {
        match g {
            Generator::AtAnchor {
                project_to_navmesh,
                drop_to_ground,
                ..
            }
            | Generator::Ring {
                project_to_navmesh,
                drop_to_ground,
                ..
            }
            | Generator::Wedge {
                project_to_navmesh,
                drop_to_ground,
                ..
            }
            | Generator::Annulus {
                project_to_navmesh,
                drop_to_ground,
                ..
            }
            | Generator::Lattice {
                project_to_navmesh,
                drop_to_ground,
                ..
            } => {
                *nav |= *project_to_navmesh;
                *physics |= drop_to_ground.is_some();
            }
            Generator::ReachLattice { drop_to_ground, .. } => {
                *reach = true;
                *physics |= drop_to_ground.is_some();
            }
            Generator::NearbyEntities { .. } => {}
            Generator::Custom {
                project_to_navmesh,
                drop_to_ground,
                ..
            } => {
                *nav |= *project_to_navmesh;
                *physics |= drop_to_ground.is_some();
            }
            Generator::Union { children, .. } => {
                for child in children {
                    walk(child, nav, reach, physics);
                }
            }
        }
    }
    /// The first `Custom` generator name with no implementation, depth first.
    fn unregistered<'a>(g: &'a Generator, env: &Env<'_>) -> Option<&'a str> {
        match g {
            Generator::Custom { name, .. } => (!env.generators.contains_key(name)).then_some(name.as_str()),
            Generator::Union { children, .. } => children.iter().find_map(|c| unregistered(c, env)),
            _ => None,
        }
    }
    if let Some(name) = unregistered(&asset.generator, env) {
        return Err(RunError::MissingSystem(format!(
            "custom generator `{name}` is not registered (SpatialQueryManager::register_generator)"
        )));
    }
    for test in &asset.tests {
        if let Test::Custom { name, .. } = test
            && !env.tests.contains_key(name)
        {
            return Err(RunError::MissingSystem(format!(
                "custom test `{name}` is not registered (SpatialQueryManager::register_test)"
            )));
        }
    }
    let (mut needs_nav, mut reach, mut needs_physics) = (false, false, false);
    walk(&asset.generator, &mut needs_nav, &mut reach, &mut needs_physics);
    let mut needs_matcher = false;
    for test in &asset.tests {
        match test {
            Test::Cast { .. } | Test::Clearance { .. } => needs_physics = true,
            Test::Route { .. } | Test::RouteAll { .. } => needs_nav = true,
            Test::Snap { target, .. } => match target {
                SnapTarget::NavMesh => needs_nav = true,
                SnapTarget::Geometry(_) => needs_physics = true,
            },
            Test::TagMatch { .. } => needs_matcher = true,
            _ => {}
        }
    }
    let has_nav = env.world.get_resource::<regia_nav::NavMesh>().is_some();
    if reach && !has_nav {
        return Err(RunError::NoNavmesh(
            "ReachLattice needs a navigation mesh (insert a regia_nav::NavMesh resource)".to_string(),
        ));
    }
    if needs_nav && !has_nav {
        return Err(RunError::MissingSystem(
            "navigation mesh (insert a regia_nav::NavMesh resource)".to_string(),
        ));
    }
    if needs_physics && env.world.get_resource::<PhysicsWorld>().is_none() {
        return Err(RunError::MissingSystem(
            "physics world (add regia_physics::PhysicsPlugin)".to_string(),
        ));
    }
    if needs_matcher && env.matcher.is_none() {
        return Err(RunError::MissingSystem(
            "tag matcher (SpatialQueryManager::set_tag_matcher)".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod panic_guard_tests {
    use super::*;

    #[test]
    fn capsule_with_bad_half_height_contains_nothing() {
        for h in [-1.0, f32::NAN, f32::INFINITY] {
            let shape = ColliderShape::Capsule {
                half_height: h,
                radius: 1.0,
                axis: Default::default(),
            };
            assert!(!contains_local(&shape, Vec3::ZERO));
        }
    }

    #[test]
    fn quantize_neighbor_probe_does_not_overflow() {
        let key = quantize(Vec3::new(1e20, -1e20, 0.0));
        assert_eq!(key.0, i64::MAX);
        assert_eq!(key.0.saturating_add(1), i64::MAX);
    }
}
