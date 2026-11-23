// SPDX-License-Identifier: MPL-2.0
//! The node registry and the built-in node library (spec 63).
//!
//! A node type implements [`NodeKind`]. The built-in types are [`BuiltinNode`] table entries (an id,
//! a settings schema, a pin function and an eval function) grouped by family in the submodules.
//! Game code adds node types with [`ProcNodeRegistry::register`].

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::data::{Bundle, DataKind, KindSet, Selector};
use crate::exec::NodeCtx;
use crate::value::SettingValue;

pub(crate) mod attr;
pub(crate) mod biome;
pub(crate) mod common;
pub(crate) mod density;
pub(crate) mod extra;
pub(crate) mod filter;
pub(crate) mod flow;
pub(crate) mod input;
pub(crate) mod items;
pub(crate) mod modules;
#[cfg(feature = "pipeline")]
pub(crate) mod plant;
#[cfg(feature = "pipeline")]
pub(crate) mod plant_mesh;
#[cfg(feature = "pipeline")]
pub(crate) mod plant_shape;
pub(crate) mod points;
pub(crate) mod road_junctions;
pub(crate) mod roads;
pub(crate) mod sample;
pub(crate) mod spawn;
pub(crate) mod spline;
pub(crate) mod terrain_ops;
pub(crate) mod tiles;
pub(crate) mod wfc;

pub use flow::{DEBUG_LOG_ENABLED, QUALITY_PINS, debug_log_message, quality_index};
pub(crate) use input::{MAX_RAYS, ray_grid};
pub use items::parse_index_list;
pub use modules::{
    MAX_EXPANDED_TERMS, ModuleDef, PatternError, Term, check_pattern_size, expanded_terms, parse_modules,
    parse_pattern,
};
pub use spline::{polygon_data_item, spline_data_item};

/// The graph domains (spec 62 § 4 `flags.domain`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Domain {
    /// World content (the default).
    #[default]
    World,
    /// Pipeline-only plant graphs (spec 68).
    Plant,
}

impl Domain {
    /// The file and agent name.
    pub fn name(self) -> &'static str {
        match self {
            Self::World => "World",
            Self::Plant => "Plant",
        }
    }

    /// Parses [`Domain::name`].
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "World" => Some(Self::World),
            "Plant" => Some(Self::Plant),
            _ => None,
        }
    }
}

/// The type of one setting, for validation and for `procgen.node_types`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SettingType {
    /// A number (f64).
    Float,
    /// An integer.
    Int,
    /// A boolean.
    Bool,
    /// A 3-vector `(x, y, z)` (a scalar broadcasts).
    Vec3,
    /// A pair of numbers `[lo, hi]`.
    Range2,
    /// A pair of 3-vectors `[lo, hi]`.
    Vec3Range,
    /// A name.
    Name,
    /// An asset path.
    Asset,
    /// An attribute selector (spec 62 § 3.5).
    Selector,
    /// One of a fixed set of identifiers.
    Enum(&'static [&'static str]),
    /// A piecewise-linear curve: `(x, y)` pairs sorted by x.
    Curve,
    /// A list of names.
    NameList,
    /// Any value; the node checks it.
    Any,
}

impl SettingType {
    /// A short type name for the registry listing.
    pub fn name(self) -> &'static str {
        match self {
            Self::Float => "f",
            Self::Int => "i",
            Self::Bool => "b",
            Self::Vec3 => "v3",
            Self::Range2 => "[f, f]",
            Self::Vec3Range => "[v3, v3]",
            Self::Name => "n",
            Self::Asset => "a",
            Self::Selector => "sel",
            Self::Enum(_) => "enum",
            Self::Curve => "curve",
            Self::NameList => "[n]",
            Self::Any => "any",
        }
    }
}

/// One setting of a node type: name, type, default (RON text), range and a doc line.
#[derive(Clone, Copy, Debug)]
pub struct SettingDesc {
    /// Setting name.
    pub name: &'static str,
    /// Value type.
    pub ty: SettingType,
    /// Default value as RON text.
    pub default: &'static str,
    /// Inclusive minimum for numbers (each component for vectors).
    pub min: Option<f64>,
    /// Inclusive maximum.
    pub max: Option<f64>,
    /// One doc line.
    pub doc: &'static str,
}

/// A setting with no range.
pub(crate) const fn set(
    name: &'static str,
    ty: SettingType,
    default: &'static str,
    doc: &'static str,
) -> SettingDesc {
    SettingDesc {
        name,
        ty,
        default,
        min: None,
        max: None,
        doc,
    }
}

/// A numeric setting with an inclusive range.
pub(crate) const fn num(
    name: &'static str,
    ty: SettingType,
    default: &'static str,
    min: f64,
    max: f64,
    doc: &'static str,
) -> SettingDesc {
    SettingDesc {
        name,
        ty,
        default,
        min: Some(min),
        max: Some(max),
        doc,
    }
}

/// One pin of a node.
#[derive(Clone, Debug, PartialEq)]
pub struct PinDesc {
    /// Pin name.
    pub name: String,
    /// Accepted (input) or produced (output) kinds.
    pub kinds: KindSet,
    /// Input pin accepting several edges (concatenated in `(from node, from pin)` order).
    pub multi: bool,
    /// Input pin that must be connected (`pin_unconnected`).
    pub required: bool,
}

/// An input pin.
pub(crate) fn pin_in(name: &str, kinds: KindSet) -> PinDesc {
    PinDesc {
        name: name.to_string(),
        kinds,
        multi: false,
        required: false,
    }
}

/// A required input pin.
pub(crate) fn pin_req(name: &str, kinds: KindSet) -> PinDesc {
    PinDesc {
        required: true,
        ..pin_in(name, kinds)
    }
}

/// A multi-edge input pin.
pub(crate) fn pin_multi(name: &str, kinds: KindSet) -> PinDesc {
    PinDesc {
        multi: true,
        ..pin_in(name, kinds)
    }
}

/// An output pin.
pub(crate) fn pin_out(name: &str, kinds: KindSet) -> PinDesc {
    pin_in(name, kinds)
}

/// The pins of a node (they may depend on its settings, e.g. `flow.match` cases).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PinSpec {
    /// Input pins in order.
    pub inputs: Vec<PinDesc>,
    /// Output pins in order.
    pub outputs: Vec<PinDesc>,
}

impl PinSpec {
    /// Pins from two lists.
    pub fn new(inputs: Vec<PinDesc>, outputs: Vec<PinDesc>) -> Self {
        Self { inputs, outputs }
    }

    /// Index of an input pin.
    pub fn input(&self, name: &str) -> Option<usize> {
        self.inputs.iter().position(|p| p.name == name)
    }

    /// Index of an output pin.
    pub fn output(&self, name: &str) -> Option<usize> {
        self.outputs.iter().position(|p| p.name == name)
    }
}

/// A node failure (spec 62 § 5.2): a stable lowercase code and a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeError {
    /// Stable code (`attr_missing`, `limit_exceeded`, `check_failed`, ...).
    pub code: String,
    /// Human-readable detail.
    pub message: String,
}

impl NodeError {
    /// An error with a code and message.
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

/// A node's resolved settings: every declared setting present (defaults filled, `Bind`s replaced).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings(pub BTreeMap<String, SettingValue>);

impl Settings {
    /// The raw value.
    pub fn get(&self, name: &str) -> Option<&SettingValue> {
        self.0.get(name)
    }

    /// A number (0 when absent or not a number; validation guarantees presence).
    pub fn f(&self, name: &str) -> f64 {
        self.get(name).and_then(SettingValue::as_f64).unwrap_or(0.0)
    }

    /// An integer.
    pub fn i(&self, name: &str) -> i64 {
        self.get(name).and_then(SettingValue::as_i64).unwrap_or(0)
    }

    /// A boolean.
    pub fn b(&self, name: &str) -> bool {
        self.get(name).and_then(SettingValue::as_bool).unwrap_or(false)
    }

    /// Text of a name, asset, selector or enum setting (`""` when absent).
    pub fn text(&self, name: &str) -> &str {
        self.get(name).and_then(SettingValue::as_text).unwrap_or("")
    }

    /// A 3-vector.
    pub fn v3(&self, name: &str) -> [f64; 3] {
        self.get(name).and_then(SettingValue::as_vec3).unwrap_or([0.0; 3])
    }

    /// A `[lo, hi]` pair.
    pub fn range2(&self, name: &str) -> [f64; 2] {
        self.get(name)
            .and_then(SettingValue::as_items)
            .and_then(|v| Some([v.first()?.as_f64()?, v.get(1)?.as_f64()?]))
            .unwrap_or([0.0, 0.0])
    }

    /// A `[lo, hi]` pair of 3-vectors.
    pub fn v3_range(&self, name: &str) -> [[f64; 3]; 2] {
        self.get(name)
            .and_then(SettingValue::as_items)
            .and_then(|v| Some([v.first()?.as_vec3()?, v.get(1)?.as_vec3()?]))
            .unwrap_or([[0.0; 3]; 2])
    }

    /// A selector (`$prev` when empty or unparsable; validation reports bad ones).
    pub fn sel(&self, name: &str) -> Selector {
        Selector::parse(self.text(name)).unwrap_or(Selector {
            item_scope: false,
            name: "$prev".to_string(),
            parts: Vec::new(),
        })
    }

    /// A curve's points.
    pub fn curve(&self, name: &str) -> Vec<(f64, f64)> {
        self.get(name).map(curve_points).unwrap_or_default()
    }

    /// A list of names.
    pub fn names(&self, name: &str) -> Vec<String> {
        self.get(name)
            .and_then(SettingValue::as_items)
            .map(|v| v.iter().filter_map(|x| x.as_text().map(str::to_string)).collect())
            .unwrap_or_default()
    }
}

fn curve_points(v: &SettingValue) -> Vec<(f64, f64)> {
    v.as_items()
        .map(|items| {
            items
                .iter()
                .filter_map(|p| {
                    let xy = p.as_items()?;
                    Some((xy.first()?.as_f64()?, xy.get(1)?.as_f64()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Evaluates a piecewise-linear curve (constant beyond the end keys; 0 for an empty curve).
pub fn eval_curve(points: &[(f64, f64)], x: f64) -> f64 {
    let Some(first) = points.first() else {
        return 0.0;
    };
    if x <= first.0 {
        return first.1;
    }
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        if x <= b.0 {
            let span = b.0 - a.0;
            return if span <= 0.0 {
                b.1
            } else {
                a.1 + (b.1 - a.1) * (x - a.0) / span
            };
        }
    }
    points.last().map_or(0.0, |p| p.1)
}

/// A node type (spec 62 § 5.1).
pub trait NodeKind: Send + Sync {
    /// The type id (`sample.ground`).
    fn id(&self) -> &str;
    /// The family (`sample`, `density`, ...), for the editor palette and `procgen.node_types`.
    fn category(&self) -> &str;
    /// One doc line.
    fn doc(&self) -> &str;
    /// The settings schema.
    fn settings(&self) -> &[SettingDesc];
    /// The pins for these settings.
    fn pins(&self, settings: &Settings) -> PinSpec;
    /// The node type's settings version (spec 62 § 4); starts at 1.
    fn settings_version(&self) -> u32 {
        1
    }
    /// Upgrades settings written by node version `from` (< [`NodeKind::settings_version`]).
    ///
    /// # Errors
    ///
    /// A message when no migrator exists for `from`.
    fn migrate_settings(
        &self,
        from: u32,
        _settings: &mut BTreeMap<String, SettingValue>,
    ) -> Result<(), String> {
        Err(format!("no settings migrator from version {from}"))
    }
    /// Checks beyond the schema: `(code, message)` per problem.
    fn validate(&self, _settings: &Settings) -> Vec<(String, String)> {
        Vec::new()
    }
    /// Domains the node type supports.
    fn domains(&self) -> &[Domain] {
        &[Domain::World, Domain::Plant]
    }
    /// Whether outputs may be cached (false for nodes that read the world or write run outputs).
    fn cacheable(&self) -> bool {
        true
    }
    /// How far from an element, in meters, the node's decision can look (spec 66 § 2): tiled runs
    /// grow each cell by twice the graph's largest reach. 0 for per-element nodes.
    fn reach_m(&self, _settings: &Settings) -> f64 {
        0.0
    }
    /// Whether a cacheable node reads world data ([`crate::exec::NodeCtx::world`]) besides its
    /// inputs: its cache key then holds [`crate::exec::WorldSnapshot::content_hash`]. Defaults to
    /// `true` (safe for host nodes); built-ins that only read their inputs answer `false`.
    fn reads_world(&self) -> bool {
        true
    }
    /// Whether the node writes run output ([`crate::exec::NodeCtx::output`], subgraph calls): such
    /// nodes run in compiled order relative to each other under a parallel schedule (spec 62 § 5.1).
    /// Defaults to `true` (safe for host nodes); built-ins answer `!cacheable`, since a node that
    /// writes run output is never cached.
    fn writes_output(&self) -> bool {
        true
    }
    /// Evaluates the node: one bundle per input pin in, one per output pin out.
    ///
    /// # Errors
    ///
    /// A [`NodeError`] fails the run (spec 62 § 5.2).
    fn eval(
        &self,
        ctx: &mut NodeCtx<'_>,
        inputs: &[Bundle],
        settings: &Settings,
    ) -> Result<Vec<Bundle>, NodeError>;

    /// [`Self::eval`] with bundles this evaluation owns, which the node may take apart (a node that
    /// rewrites its input in place then does not copy it). The default calls [`Self::eval`].
    ///
    /// # Errors
    ///
    /// A [`NodeError`] fails the run (spec 62 § 5.2).
    fn eval_owned(
        &self,
        ctx: &mut NodeCtx<'_>,
        inputs: &mut [Bundle],
        settings: &Settings,
    ) -> Result<Vec<Bundle>, NodeError> {
        self.eval(ctx, inputs, settings)
    }
}

/// Eval function of a built-in node.
pub(crate) type EvalFn = fn(&mut NodeCtx<'_>, &[Bundle], &Settings) -> Result<Vec<Bundle>, NodeError>;
/// Eval function of a built-in node that takes its inputs apart ([`NodeKind::eval_owned`]).
pub(crate) type OwnedEvalFn =
    fn(&mut NodeCtx<'_>, &mut [Bundle], &Settings) -> Result<Vec<Bundle>, NodeError>;
/// Pin function of a built-in node.
pub(crate) type PinsFn = fn(&Settings) -> PinSpec;
/// Extra validation of a built-in node.
pub(crate) type ValidateFn = fn(&Settings) -> Vec<(String, String)>;
/// Declared reach of a built-in node (spec 66 § 2).
pub(crate) type ReachFn = fn(&Settings) -> f64;

/// How a built-in node evaluates.
#[derive(Clone, Copy)]
pub(crate) enum EvalKind {
    /// It reads its inputs.
    Shared(EvalFn),
    /// It takes its inputs apart: a node that rewrites its input in place ([`crate::data::Column`]).
    InPlace(OwnedEvalFn),
}

/// A built-in node type: a table entry with function pointers.
#[derive(Clone, Copy)]
pub struct BuiltinNode {
    pub(crate) id: &'static str,
    pub(crate) category: &'static str,
    pub(crate) doc: &'static str,
    pub(crate) settings: &'static [SettingDesc],
    pub(crate) pins: PinsFn,
    pub(crate) eval: EvalKind,
    pub(crate) validate: Option<ValidateFn>,
    pub(crate) cacheable: bool,
    pub(crate) world_only: bool,
    pub(crate) reach: Option<ReachFn>,
    pub(crate) plant_only: bool,
    pub(crate) reads_world: bool,
}

impl BuiltinNode {
    /// A cacheable node supported in both domains.
    pub(crate) const fn new(
        id: &'static str,
        category: &'static str,
        doc: &'static str,
        settings: &'static [SettingDesc],
        pins: PinsFn,
        eval: EvalFn,
    ) -> Self {
        Self {
            id,
            category,
            doc,
            settings,
            pins,
            eval: EvalKind::Shared(eval),
            validate: None,
            cacheable: true,
            world_only: false,
            reach: None,
            plant_only: false,
            reads_world: false,
        }
    }

    /// [`Self::new`] for a node that takes its inputs apart ([`crate::nodes::take_input`]): the
    /// bundles it writes are not copied when nothing else holds them.
    pub(crate) const fn in_place(
        id: &'static str,
        category: &'static str,
        doc: &'static str,
        settings: &'static [SettingDesc],
        pins: PinsFn,
        eval: OwnedEvalFn,
    ) -> Self {
        Self {
            id,
            category,
            doc,
            settings,
            pins,
            eval: EvalKind::InPlace(eval),
            validate: None,
            cacheable: true,
            world_only: false,
            reach: None,
            plant_only: false,
            reads_world: false,
        }
    }

    /// Declares how far from an element the node's decisions look (spec 66 § 2).
    pub(crate) const fn reaching(mut self, f: ReachFn) -> Self {
        self.reach = Some(f);
        self
    }

    /// Marks a cacheable node as reading world data (ground, meshes): its cache key holds the
    /// snapshot's content hash.
    pub(crate) const fn reads_world_data(mut self) -> Self {
        self.reads_world = true;
        self
    }

    /// Marks the node as not cacheable (world reads, run outputs).
    pub(crate) const fn uncached(mut self) -> Self {
        self.cacheable = false;
        self
    }

    /// Restricts the node to the `World` domain.
    pub(crate) const fn world(mut self) -> Self {
        self.world_only = true;
        self
    }

    /// Restricts the node to the `Plant` domain (spec 68 § 2).
    #[cfg(feature = "pipeline")]
    pub(crate) const fn plant(mut self) -> Self {
        self.plant_only = true;
        self
    }

    /// Adds an extra validation function.
    pub(crate) const fn checked(mut self, f: ValidateFn) -> Self {
        self.validate = Some(f);
        self
    }
}

impl NodeKind for BuiltinNode {
    fn id(&self) -> &str {
        self.id
    }
    fn category(&self) -> &str {
        self.category
    }
    fn doc(&self) -> &str {
        self.doc
    }
    fn settings(&self) -> &[SettingDesc] {
        self.settings
    }
    fn pins(&self, settings: &Settings) -> PinSpec {
        (self.pins)(settings)
    }
    fn validate(&self, settings: &Settings) -> Vec<(String, String)> {
        self.validate.map(|f| f(settings)).unwrap_or_default()
    }
    fn domains(&self) -> &[Domain] {
        if self.world_only {
            &[Domain::World]
        } else if self.plant_only {
            &[Domain::Plant]
        } else {
            &[Domain::World, Domain::Plant]
        }
    }
    fn cacheable(&self) -> bool {
        self.cacheable
    }
    fn reach_m(&self, settings: &Settings) -> f64 {
        self.reach.map_or(0.0, |f| f(settings))
    }
    fn reads_world(&self) -> bool {
        self.reads_world
    }
    fn writes_output(&self) -> bool {
        !self.cacheable
    }
    fn eval(
        &self,
        ctx: &mut NodeCtx<'_>,
        inputs: &[Bundle],
        settings: &Settings,
    ) -> Result<Vec<Bundle>, NodeError> {
        match self.eval {
            EvalKind::Shared(f) => f(ctx, inputs, settings),
            EvalKind::InPlace(f) => f(ctx, &mut inputs.to_vec(), settings),
        }
    }
    fn eval_owned(
        &self,
        ctx: &mut NodeCtx<'_>,
        inputs: &mut [Bundle],
        settings: &Settings,
    ) -> Result<Vec<Bundle>, NodeError> {
        match self.eval {
            EvalKind::Shared(f) => f(ctx, inputs, settings),
            EvalKind::InPlace(f) => f(ctx, inputs, settings),
        }
    }
}

/// Node type ids handled by the compiler itself: `note` (no pins, excluded) and `reroute`
/// (collapsed before hashing and evaluation).
pub const EDITOR_AIDS: [&str; 2] = ["note", "reroute"];

/// The registry of node types (`procgen.node_types`).
#[derive(Clone, Default)]
pub struct ProcNodeRegistry {
    kinds: BTreeMap<String, Arc<dyn NodeKind>>,
}

impl ProcNodeRegistry {
    /// An empty registry.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The registry with every built-in node type of spec 63 that is implemented (ADR-1640).
    pub fn with_builtins() -> Self {
        let mut r = Self::empty();
        for family in [
            input::NODES,
            sample::NODES,
            density::NODES,
            points::NODES,
            filter::NODES,
            items::NODES,
            attr::NODES,
            flow::NODES,
            spawn::NODES,
            extra::NODES,
            spline::NODES,
            modules::NODES,
            roads::NODES,
            road_junctions::NODES,
            biome::NODES,
            tiles::NODES,
            wfc::NODES,
            terrain_ops::NODES,
        ] {
            for node in family {
                r.kinds.insert(node.id.to_string(), Arc::new(*node));
            }
        }
        // Plant node types exist only in pipeline builds (spec 68 § 2).
        #[cfg(feature = "pipeline")]
        for family in [plant::NODES, plant_mesh::NODES, plant_shape::NODES] {
            for node in family {
                r.kinds.insert(node.id.to_string(), Arc::new(*node));
            }
        }
        r
    }

    /// Adds (or replaces) a node type: the extension point for game code (spec 62 § 14).
    pub fn register(&mut self, kind: Arc<dyn NodeKind>) {
        self.kinds.insert(kind.id().to_string(), kind);
    }

    /// A node type by id.
    pub fn get(&self, id: &str) -> Option<&Arc<dyn NodeKind>> {
        self.kinds.get(id)
    }

    /// Every node type in id order.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn NodeKind>> {
        self.kinds.values()
    }

    /// Number of registered types.
    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }
}

/// Shorthand kind sets used by the families.
pub(crate) const POINTS: KindSet = KindSet::POINTS;
/// Spatial kinds (region pins).
pub(crate) const SPATIAL: KindSet = KindSet::SPATIAL;
/// Any kind.
pub(crate) const ANY: KindSet = KindSet::ANY;
/// Tables.
pub(crate) const TABLE: KindSet = KindSet::TABLE;

/// `in` → `out` for points.
pub(crate) fn pins_points_inout(_: &Settings) -> PinSpec {
    PinSpec::new(vec![pin_req("in", POINTS)], vec![pin_out("out", POINTS)])
}

/// `in` → `out` for any kind.
pub(crate) fn pins_any_inout(_: &Settings) -> PinSpec {
    PinSpec::new(vec![pin_req("in", ANY)], vec![pin_out("out", ANY)])
}

/// `in` → `inside`, `outside`.
pub(crate) fn pins_split(kinds: KindSet) -> PinSpec {
    PinSpec::new(
        vec![pin_req("in", kinds)],
        vec![pin_out("inside", kinds), pin_out("outside", kinds)],
    )
}

/// The input bundle of pin `k` (empty when absent).
pub(crate) fn input(inputs: &[Bundle], k: usize) -> Bundle {
    inputs.get(k).cloned().unwrap_or_default()
}

/// The items of input pin `k`, borrowed (empty when absent): for nodes that only read their
/// input, so a 1 M-point set is not deep-copied just to be iterated (spec 63 § 15 budgets).
pub(crate) fn input_items(inputs: &[Bundle], k: usize) -> &[crate::data::DataItem] {
    inputs.get(k).map_or(&[], |b| b.0.as_slice())
}

/// Input pin `k`, taken out of a slice this evaluation owns (empty when absent).
pub(crate) fn take_input(inputs: &mut [Bundle], k: usize) -> Bundle {
    inputs.get_mut(k).map(std::mem::take).unwrap_or_default()
}

/// A kind set of one kind.
pub(crate) fn kind(k: DataKind) -> KindSet {
    KindSet::of(&[k])
}
