// SPDX-License-Identifier: MPL-2.0
//! Choosers: ordered tables of rows whose condition cells are tested against a context of named
//! input values; evaluation returns the result of the row that matches, with an optional trace of
//! every decision. Used to choose a clip, a montage, a sound cue, a loot entry or any value without
//! an `if` ladder.
//!
//! Specification: `docs/specs/51-curve-tables-and-choosers.md` §§ 2.2-2.7, 4.3 (ADR-1649);
//! guide: `book/agents/choosers.md`; format: `docs/formats/chooser-ron.md`.
//!
//! - [`ChooserTable`] (`.chooser.ron`, format `chooser` v1): a [`Schema`] of declared inputs and
//!   outputs, [`Column`]s (gate, rank, draw and emit kinds, [`ColumnKind`]), [`Row`]s with one
//!   [`Cell`] per column, an optional [`CatchAll`], a [`Mode`] and a [`ResultType`].
//! - [`ChooserContext`]: the per-evaluation input values and the outputs the winning row writes.
//! - [`ChooserTable::evaluate`] (pure for every mode except `Weighted`, which takes exactly one
//!   [`Rng::f32`](regia_core::math::Rng::f32) per draw), [`ChooserTable::evaluate_all`] and
//!   [`ChooserTable::evaluate_traced`] (a [`ChooserTrace`] of every row and cell verdict).
//! - [`Choosers`] (a resource) holds loaded tables by asset path and resolves nested `Chooser(..)`
//!   results; [`ChooserBinding`] components re-evaluate per entity and raise [`ChooserPicked`].
//!
//! ```
//! use regia_core::math::Rng;
//! use regia_framework::chooser::{ChooserContext, ChooserTable, ChooserValue, NoChoosers, ResultEntry};
//!
//! let table = ChooserTable::from_ron_str(r#"(
//!     format_version: 1,
//!     result: Asset("cue"),
//!     inputs: { "armed": Bool },
//!     columns: [(id: "armed", input: "armed", kind: Bool)],
//!     rows: [
//!         (id: "armed", cells: { "armed": True }, result: Asset("audio/armed.cue.ron")),
//!         (id: "unarmed", cells: {}, result: Asset("audio/unarmed.cue.ron")),
//!     ],
//! )"#).expect("valid chooser");
//! let mut ctx = ChooserContext::default();
//! ctx.set("armed", ChooserValue::Bool(false));
//! let picked = table.evaluate(&NoChoosers, &mut ctx, &mut Rng::new(0), None).expect("a row");
//! assert_eq!(picked.row.as_deref(), Some("unarmed"));
//! assert_eq!(picked.result, ResultEntry::Asset("audio/unarmed.cue.ron".into()));
//! ```

mod bridge;
mod cooked;
mod eval;
mod format;
mod runtime;
mod validate;

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::tags::{TagContainer, TagQuery};

#[cfg(feature = "client")]
pub use bridge::cue_bridge;
pub use bridge::{ChooserBridgePlugin, asset_stem, montage_bridge};
pub use cooked::{MAX_COOKED_NESTING, chooser_cooked_schema_fingerprint};
pub use eval::{CellTrace, ChooserTrace, RowOutcome, RowTrace, TraceDetail, Verdict};
pub use format::{
    cell_ron, chooser_migration_sources, chooser_schema_fingerprint, parse_cell, parse_column_kind,
    parse_result, parse_value, result_ron, value_ron,
};
pub use runtime::{
    ChooserAppExt, ChooserBinding, ChooserPicked, ChooserPlugin, ChooserProviders, ChooserRegistrations,
    ChooserSet, ChooserSlot, ChooserStats, ChooserTraceLog, Choosers, LoggedTrace, PendingChooserReloads,
    ProviderFn, Reeval, apply_chooser_reloads, collect_chooser_changes, evaluate_bindings,
    load_registered_choosers,
};

/// Current `.chooser.ron` format version (`chooser` v1).
pub const CHOOSER_FORMAT_VERSION: u32 = 1;

/// Current version of the cooked (pak) form ([`ChooserTable::to_cooked`]).
pub const CHOOSER_COOKED_VERSION: u32 = 1;

/// Type path of a cooked chooser in the asset manifest and the cooked header.
pub const CHOOSER_TYPE_PATH: &str = "regia_framework::chooser::ChooserTable";

/// Most rows one table may hold.
pub const MAX_ROWS: usize = 4096;

/// Most columns one table may hold.
pub const MAX_COLUMNS: usize = 64;

/// Most declared inputs, and most declared outputs, one table may hold.
pub const MAX_PARAMS: usize = 64;

/// Deepest nesting of `Chooser(..)` / `Inline(..)` results.
pub const MAX_DEPTH: u32 = 8;

/// Most recent picks a [`RecentPicks`] memory keeps (the largest [`Recency::window`]).
pub const MAX_RECENCY_WINDOW: u32 = 16;

/// Absolute tolerance under which two `Best` costs count as equal (the earlier row wins).
pub const COST_EPSILON: f32 = 1e-6;

/// The kind of a declared input or output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValueKind {
    /// `true` / `false`.
    Bool,
    /// A 64-bit integer.
    Int,
    /// A 32-bit float.
    Float,
    /// One of the listed variant names.
    Enum(Vec<String>),
    /// A free string.
    Name,
    /// A gameplay tag container.
    Tags,
    /// An asset path.
    Asset,
}

impl ValueKind {
    /// The lowercase kind name (`bool`, `int`, `float`, `enum`, `name`, `tags`, `asset`).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int => "int",
            Self::Float => "float",
            Self::Enum(_) => "enum",
            Self::Name => "name",
            Self::Tags => "tags",
            Self::Asset => "asset",
        }
    }

    /// Whether `value` is of this kind (an `Enum` value must be one of the variants).
    #[must_use]
    pub fn admits(&self, value: &ChooserValue) -> bool {
        match (self, value) {
            (Self::Enum(variants), ChooserValue::Enum(v)) => variants.iter().any(|x| x == v),
            (Self::Bool, ChooserValue::Bool(_))
            | (Self::Int, ChooserValue::Int(_))
            | (Self::Float, ChooserValue::Float(_))
            | (Self::Name, ChooserValue::Name(_))
            | (Self::Tags, ChooserValue::Tags(_))
            | (Self::Asset, ChooserValue::Asset(_)) => true,
            _ => false,
        }
    }
}

/// One input, output or inline result value.
#[derive(Debug, Clone, PartialEq)]
pub enum ChooserValue {
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A float.
    Float(f32),
    /// An enum variant name.
    Enum(String),
    /// A free string (a loot table name, a localization key).
    Name(String),
    /// A tag container.
    Tags(TagContainer),
    /// An asset path.
    Asset(String),
}

impl ChooserValue {
    /// Whether two values are the same, comparing floats by their bits (so `NaN` equals the same
    /// `NaN` and `0.0` differs from `-0.0`); the binding `OnChange` test.
    #[must_use]
    pub fn identical(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Float(a), Self::Float(b)) => a.to_bits() == b.to_bits(),
            _ => self == other,
        }
    }

    /// The lowercase kind name.
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::Float(_) => "float",
            Self::Enum(_) => "enum",
            Self::Name(_) => "name",
            Self::Tags(_) => "tags",
            Self::Asset(_) => "asset",
        }
    }
}

/// The asset kind of a path: the second-to-last extension when the last is `ron`
/// (`anim/hit.montage.ron` is `montage`), else the last extension (`fx/boom.wav` is `wav`).
#[must_use]
pub fn asset_kind(path: &str) -> &str {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let Some((stem, last)) = file.rsplit_once('.') else {
        return "";
    };
    if last == "ron"
        && let Some((_, kind)) = stem.rsplit_once('.')
    {
        return kind;
    }
    last
}

/// The per-evaluation values: inputs the caller sets (read only during evaluation) and outputs
/// the winning row writes (never visible to columns of the same evaluation).
///
/// Inputs stay in the map across evaluations. `begin_fill` stamps an epoch and `end_fill` drops
/// keys the provider did not set, so a stable key is overwritten in place.
#[derive(Clone, Default)]
pub struct ChooserContext {
    inputs: BTreeMap<String, InputSlot>,
    outputs: BTreeMap<String, ChooserValue>,
    epoch: u64,
}

/// One input plus the epoch that last wrote it. Equality of a context compares values only.
#[derive(Clone, Debug)]
struct InputSlot {
    value: ChooserValue,
    epoch: u64,
}

thread_local! {
    static CONTEXT_KEY_INSERTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many chooser context keys have been inserted on this thread.
///
/// A later `set` of a name that is already present overwrites the value and does not count.
#[cfg(test)]
pub(crate) fn context_key_inserts() -> u64 {
    CONTEXT_KEY_INSERTS.with(|count| count.get())
}

impl PartialEq for ChooserContext {
    fn eq(&self, other: &Self) -> bool {
        self.outputs == other.outputs
            && self.inputs.len() == other.inputs.len()
            && self
                .inputs
                .iter()
                .zip(other.inputs.iter())
                .all(|((ka, va), (kb, vb))| ka == kb && va.value == vb.value)
    }
}

impl std::fmt::Debug for ChooserContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inputs: BTreeMap<&str, &ChooserValue> = self
            .inputs
            .iter()
            .map(|(key, slot)| (key.as_str(), &slot.value))
            .collect();
        f.debug_struct("ChooserContext")
            .field("inputs", &inputs)
            .field("outputs", &self.outputs)
            .finish()
    }
}

impl ChooserContext {
    /// Sets input `name`.
    pub fn set(&mut self, name: &str, value: ChooserValue) {
        if let Some(slot) = self.inputs.get_mut(name) {
            slot.value = value;
            slot.epoch = self.epoch;
            return;
        }
        CONTEXT_KEY_INSERTS.with(|count| count.set(count.get().saturating_add(1)));
        self.inputs.insert(
            name.to_owned(),
            InputSlot {
                value,
                epoch: self.epoch,
            },
        );
    }

    /// Removes input `name` (it then reads as absent).
    pub fn unset(&mut self, name: &str) {
        self.inputs.remove(name);
    }

    /// Input `name`, if set.
    #[must_use]
    pub fn input(&self, name: &str) -> Option<&ChooserValue> {
        self.inputs.get(name).map(|slot| &slot.value)
    }

    /// Output `name`, if the last evaluation wrote it.
    #[must_use]
    pub fn output(&self, name: &str) -> Option<&ChooserValue> {
        self.outputs.get(name)
    }

    /// Every input, sorted by name.
    pub fn inputs(&self) -> impl Iterator<Item = (&str, &ChooserValue)> {
        self.inputs.iter().map(|(k, slot)| (k.as_str(), &slot.value))
    }

    /// Every output, sorted by name.
    pub fn outputs(&self) -> impl Iterator<Item = (&str, &ChooserValue)> {
        self.outputs.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Whether `other` has the same inputs, floats compared by bits (outputs ignored).
    #[must_use]
    pub fn same_inputs(&self, other: &Self) -> bool {
        self.inputs.len() == other.inputs.len()
            && self
                .inputs
                .iter()
                .zip(other.inputs.iter())
                .all(|((ka, va), (kb, vb))| ka == kb && va.value.identical(&vb.value))
    }

    pub(crate) fn clear_outputs(&mut self) {
        self.outputs.clear();
    }

    pub(crate) fn write_output(&mut self, name: &str, value: ChooserValue) {
        self.outputs.insert(name.to_owned(), value);
    }

    /// Starts a provider fill. The epoch wraps by clearing the map, so a key is never stamped `0`
    /// after a wrap (callers that `set` without a fill keep epoch `0`).
    pub(crate) fn begin_fill(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.inputs.clear();
            self.epoch = 1;
        }
        self.outputs.clear();
    }

    /// Drops inputs the provider did not set during the current fill.
    pub(crate) fn end_fill(&mut self) {
        let epoch = self.epoch;
        self.inputs.retain(|_, slot| slot.epoch == epoch);
    }
}

/// How a table selects among the rows that pass every gate (the candidates).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Mode {
    /// The first candidate in row order.
    #[default]
    First,
    /// The candidate with the lowest total cost; ties (within [`COST_EPSILON`]) go to the earlier row.
    Best,
    /// A random draw proportional to the `Weight` cells, optionally among the best-cost band.
    Weighted,
    /// Every candidate in row order ([`ChooserTable::evaluate_all`]).
    All,
}

impl Mode {
    /// The lowercase name (`first`, `best`, `weighted`, `all`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Best => "best",
            Self::Weighted => "weighted",
            Self::All => "all",
        }
    }

    /// Parses the lowercase name or the RON spelling.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "first" => Some(Self::First),
            "best" => Some(Self::Best),
            "weighted" => Some(Self::Weighted),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// The kind of an inline `Value(..)` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScalarKind {
    /// A boolean.
    Bool,
    /// An integer.
    Int,
    /// A float.
    Float,
    /// A free string.
    Name,
    /// A tag container.
    Tags,
}

impl ScalarKind {
    fn value_kind(self) -> ValueKind {
        match self {
            Self::Bool => ValueKind::Bool,
            Self::Int => ValueKind::Int,
            Self::Float => ValueKind::Float,
            Self::Name => ValueKind::Name,
            Self::Tags => ValueKind::Tags,
        }
    }
}

/// What a table's results are; every result entry must agree with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResultType {
    /// Asset references of a free kind tag (`clip`, `montage`, `cue`, `prefab`; `any` accepts every
    /// kind).
    Asset(String),
    /// Inline values of one kind.
    Value(ScalarKind),
    /// No result; the table only writes outputs (entries must be `Nothing`).
    OutputOnly,
}

/// One result entry, per row and for the catch-all.
#[derive(Debug, Clone, PartialEq)]
pub enum ResultEntry {
    /// An asset reference; resolved or loaded by the caller, never by the evaluator.
    Asset(String),
    /// An inline value.
    Value(ChooserValue),
    /// A nested chooser asset, evaluated only when this row is selected.
    Chooser(String),
    /// A nested table stored in this file, evaluated only when this row is selected.
    Inline(Box<ChooserTable>),
    /// The row yields no result (output-only effects).
    Nothing,
}

/// Declared inputs and outputs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Schema {
    /// Inputs columns may read, by name.
    pub inputs: BTreeMap<String, ValueKind>,
    /// Outputs `Set*` columns may write, by name.
    pub outputs: BTreeMap<String, ValueKind>,
}

/// A column's kind: gate (filters), rank (adds cost), draw (weight) or emit (writes an output).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ColumnKind {
    /// Gate on a `Bool` input: `Any`, `True`, `False`.
    Bool,
    /// Gate on an `Int` input: `Any` or `(min, max)` inclusive.
    IntRange,
    /// Gate on a `Float` input: `Any` or `(min, max)` inclusive.
    FloatRange,
    /// Gate on an `Enum` input: `Any`, `Is(v)`, `IsNot(v)`, `OneOf([..])`.
    Enum,
    /// Gate on a `Name` input: `Any` or a string.
    Name,
    /// Gate on a `Tags` input: `Any` or a tag query.
    Tags,
    /// Gate on an `Asset` input: `Any`, `Is(path)`, `Kind(kind)`.
    Asset,
    /// Rank on a `Float` input: `Any` (cost 0) or a target; adds
    /// `influence * min(gap, max_distance) / max_distance`.
    FloatDistance {
        /// Largest cost the column adds (>= 0, default 1).
        #[serde(default = "one")]
        influence: f32,
        /// Gap at which the cost saturates (> 0).
        max_distance: f32,
        /// Circular range `(lo, hi)` for angles: the gap is the shorter way round.
        #[serde(default)]
        wrap: Option<(f32, f32)>,
        /// Also reject rows whose gap exceeds `max_distance`.
        #[serde(default)]
        drop_beyond: bool,
    },
    /// Draw weight (`f32 >= 0`, default 1) for `Weighted` mode; at most one per table.
    Weight,
    /// Emit a `Bool` output.
    SetBool,
    /// Emit an `Int` output.
    SetInt,
    /// Emit a `Float` output.
    SetFloat,
    /// Emit an `Enum` output.
    SetEnum,
    /// Emit a `Name` output.
    SetName,
    /// Emit a `Tags` output.
    SetTags,
    /// Emit an `Asset` output.
    SetAsset,
}

fn one() -> f32 {
    1.0
}

/// The four column classes of spec 51 § 2.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnClass {
    /// Filters rows.
    Gate,
    /// Adds cost.
    Rank,
    /// Weights the draw.
    Draw,
    /// Writes an output.
    Emit,
}

impl ColumnKind {
    /// The column's class.
    #[must_use]
    pub fn class(&self) -> ColumnClass {
        match self {
            Self::Bool
            | Self::IntRange
            | Self::FloatRange
            | Self::Enum
            | Self::Name
            | Self::Tags
            | Self::Asset => ColumnClass::Gate,
            Self::FloatDistance { .. } => ColumnClass::Rank,
            Self::Weight => ColumnClass::Draw,
            _ => ColumnClass::Emit,
        }
    }

    /// The RON name of the kind (`Bool`, `FloatDistance`, `SetFloat`, ...).
    #[must_use]
    pub fn ron_name(&self) -> &'static str {
        match self {
            Self::Bool => "Bool",
            Self::IntRange => "IntRange",
            Self::FloatRange => "FloatRange",
            Self::Enum => "Enum",
            Self::Name => "Name",
            Self::Tags => "Tags",
            Self::Asset => "Asset",
            Self::FloatDistance { .. } => "FloatDistance",
            Self::Weight => "Weight",
            Self::SetBool => "SetBool",
            Self::SetInt => "SetInt",
            Self::SetFloat => "SetFloat",
            Self::SetEnum => "SetEnum",
            Self::SetName => "SetName",
            Self::SetTags => "SetTags",
            Self::SetAsset => "SetAsset",
        }
    }

    /// Whether a value of `kind` fits this column (the input kind for gate and rank columns, the
    /// output kind for emit columns).
    #[must_use]
    pub fn fits(&self, kind: &ValueKind) -> bool {
        matches!(
            (self, kind),
            (Self::Bool | Self::SetBool, ValueKind::Bool)
                | (Self::IntRange | Self::SetInt, ValueKind::Int)
                | (
                    Self::FloatRange | Self::FloatDistance { .. } | Self::SetFloat,
                    ValueKind::Float
                )
                | (Self::Enum | Self::SetEnum, ValueKind::Enum(_))
                | (Self::Name | Self::SetName, ValueKind::Name)
                | (Self::Tags | Self::SetTags, ValueKind::Tags)
                | (Self::Asset | Self::SetAsset, ValueKind::Asset)
        )
    }
}

/// One column: a condition or action applied to every row.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// Unique id; rows key their cells by it.
    pub id: String,
    /// The input a gate or rank column reads.
    pub input: Option<String>,
    /// The output an emit column writes.
    pub output: Option<String>,
    /// What the column does.
    pub kind: ColumnKind,
    /// Circular range `(lo, hi)` for `IntRange` / `FloatRange`: a range may run past `hi` back to
    /// `lo` (`min > max`).
    pub wrap: Option<(f64, f64)>,
    /// `Name` columns: compare case-sensitively (default `true`).
    pub case_sensitive: bool,
    /// A disabled column counts as `Any` for every row (and an emit column writes nothing).
    pub disabled: bool,
}

impl Column {
    /// A column with default options.
    #[must_use]
    pub fn new(id: &str, kind: ColumnKind) -> Self {
        Self {
            id: id.to_owned(),
            input: None,
            output: None,
            kind,
            wrap: None,
            case_sensitive: true,
            disabled: false,
        }
    }
}

/// One cell: a row's value for one column. Which variants a column takes depends on its kind.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// Matches anything (cost 0 in a rank column).
    Any,
    /// `True` / `False` (`Bool` column).
    Bool(bool),
    /// Inclusive range (`IntRange` column).
    IntRange(i64, i64),
    /// Inclusive range (`FloatRange` column).
    FloatRange(f32, f32),
    /// The variant or asset path equals (`Enum`, `Asset` columns).
    Is(String),
    /// The variant differs (`Enum` column).
    IsNot(String),
    /// The variant is one of these (`Enum` column).
    OneOf(Vec<String>),
    /// The asset's kind equals (`Asset` column; see [`asset_kind`]).
    Kind(String),
    /// Equality with a string (`Name` column).
    Name(String),
    /// A tag query over the input container (`Tags` column).
    Tags(TagQuery),
    /// Distance target (`FloatDistance` column).
    Target(f32),
    /// Draw weight (`Weight` column).
    Weight(f32),
    /// The value to write (emit columns).
    Value(ChooserValue),
}

/// A row's cells by column id, in id order: the part of a map a row uses, in one exact-size
/// allocation. A `BTreeMap` allocates a node of eleven entries for a one-cell row, which alone is
/// three times spec 51 § 10's memory budget over a 4,096-row table (ADR-2012).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RowCells(Vec<(String, Cell)>);

/// A walk over a [`RowCells`], in column id order.
pub type RowCellsIter<'a> =
    std::iter::Map<std::slice::Iter<'a, (String, Cell)>, fn(&'a (String, Cell)) -> (&'a String, &'a Cell)>;

impl RowCells {
    /// No cells.
    #[must_use]
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// The number of cells.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the row has no cells.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn position(&self, column: &str) -> Result<usize, usize> {
        self.0.binary_search_by(|(id, _)| id.as_str().cmp(column))
    }

    /// The cell of `column`.
    #[must_use]
    pub fn get(&self, column: &str) -> Option<&Cell> {
        let at = self.position(column).ok()?;
        self.0.get(at).map(|(_, cell)| cell)
    }

    /// Whether `column` has a cell.
    #[must_use]
    pub fn contains_key(&self, column: &str) -> bool {
        self.position(column).is_ok()
    }

    /// Sets the cell of `column`; returns the one it replaced.
    pub fn insert(&mut self, column: String, cell: Cell) -> Option<Cell> {
        match self.position(&column) {
            Ok(at) => self.0.get_mut(at).map(|(_, old)| std::mem::replace(old, cell)),
            Err(at) => {
                self.0.insert(at, (column, cell));
                None
            }
        }
    }

    /// Removes the cell of `column` and returns it.
    pub fn remove(&mut self, column: &str) -> Option<Cell> {
        let at = self.position(column).ok()?;
        Some(self.0.remove(at).1)
    }

    /// Every `(column, cell)`, in column id order.
    pub fn iter(&self) -> RowCellsIter<'_> {
        fn split(cell: &(String, Cell)) -> (&String, &Cell) {
            (&cell.0, &cell.1)
        }
        self.0.iter().map(split as fn(_) -> _)
    }

    /// One sort over cells in any order, for a row read whole: a repeated column is the `Err`.
    pub(crate) fn from_unsorted(mut cells: Vec<(String, Cell)>) -> Result<Self, String> {
        cells.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        if let Some(pair) = cells.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(pair[0].0.clone());
        }
        cells.shrink_to_fit();
        Ok(Self(cells))
    }
}

impl<'a> IntoIterator for &'a RowCells {
    type Item = (&'a String, &'a Cell);
    type IntoIter = RowCellsIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// As for a map, a repeated column keeps the last cell.
impl FromIterator<(String, Cell)> for RowCells {
    fn from_iter<I: IntoIterator<Item = (String, Cell)>>(cells: I) -> Self {
        let mut cells: Vec<(String, Cell)> = cells.into_iter().collect();
        cells.sort_by(|a, b| a.0.cmp(&b.0));
        cells.dedup_by(|later, kept| {
            let repeated = later.0 == kept.0;
            if repeated {
                std::mem::swap(later, kept);
            }
            repeated
        });
        cells.shrink_to_fit();
        Self(cells)
    }
}

impl From<BTreeMap<String, Cell>> for RowCells {
    fn from(cells: BTreeMap<String, Cell>) -> Self {
        Self(cells.into_iter().collect())
    }
}

/// One row of a table.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Unique id, stable across edits (tests and traces refer to it).
    pub id: String,
    /// A disabled row is skipped.
    pub enabled: bool,
    /// Cells by column id; a missing cell is `Any` (weight 1, no output write).
    pub cells: RowCells,
    /// What the row yields when selected.
    pub result: ResultEntry,
    /// A note for authors.
    pub note: String,
}

impl Row {
    /// An enabled row with no cells.
    #[must_use]
    pub fn new(id: &str, result: ResultEntry) -> Self {
        Self {
            id: id.to_owned(),
            enabled: true,
            cells: RowCells::new(),
            result,
            note: String::new(),
        }
    }

    /// The cell for `column` (`Any` when absent).
    #[must_use]
    pub fn cell(&self, column: &str) -> &Cell {
        self.cells.get(column).unwrap_or(&Cell::Any)
    }
}

/// The result used when no row is selected, and the outputs it writes.
#[derive(Debug, Clone, PartialEq)]
pub struct CatchAll {
    /// The result.
    pub result: ResultEntry,
    /// Outputs written when the catch-all answers.
    pub outputs: BTreeMap<String, ChooserValue>,
}

/// Repeat avoidance for `Weighted` mode: a candidate among the last `window` picks has its weight
/// multiplied by `penalty` (0 excludes it, 0.25 makes a repeat a quarter as likely).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Recency {
    /// How many recent picks count, `1..=16`.
    pub window: u32,
    /// Weight multiplier in `[0, 1]`.
    #[serde(default)]
    pub penalty: f32,
}

/// The caller-owned memory of recent `Weighted` picks (per character or per context, never global).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentPicks {
    /// Row ids, oldest first; at most [`MAX_RECENCY_WINDOW`].
    pub rows: VecDeque<String>,
}

impl RecentPicks {
    /// Whether `row` is among the last `window` picks.
    #[must_use]
    pub fn contains_recent(&self, row: &str, window: u32) -> bool {
        self.rows.iter().rev().take(window as usize).any(|r| r == row)
    }

    /// Records a pick, dropping the oldest beyond [`MAX_RECENCY_WINDOW`].
    pub fn record(&mut self, row: &str) {
        self.rows.push_back(row.to_owned());
        while self.rows.len() > MAX_RECENCY_WINDOW as usize {
            self.rows.pop_front();
        }
    }
}

/// A chooser table (`.chooser.ron`). See the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct ChooserTable {
    /// Declared inputs and outputs.
    pub schema: Schema,
    /// Selection mode.
    pub mode: Mode,
    /// What the results are.
    pub result_type: ResultType,
    /// Columns in evaluation order.
    pub columns: Vec<Column>,
    /// Rows in evaluation order.
    pub rows: Vec<Row>,
    /// The result when no row is selected.
    pub catch_all: Option<CatchAll>,
    /// `Weighted`: only candidates with `cost <= best_cost + near_best` take part (negative counts as 0).
    pub near_best: Option<f32>,
    /// `Weighted`: repeat avoidance.
    pub recency: Option<Recency>,
}

impl ChooserTable {
    /// An empty table of `result_type`.
    #[must_use]
    pub fn new(mode: Mode, result_type: ResultType) -> Self {
        Self {
            schema: Schema::default(),
            mode,
            result_type,
            columns: Vec::new(),
            rows: Vec::new(),
            catch_all: None,
            near_best: None,
            recency: None,
        }
    }

    /// The column with id `id`.
    #[must_use]
    pub fn column(&self, id: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.id == id)
    }

    /// The row with id `id`.
    #[must_use]
    pub fn row(&self, id: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.id == id)
    }
}

/// How serious a [`ChooserIssue`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IssueSeverity {
    /// The table is wrong; it does not load (a reload with errors is not applied).
    Error,
    /// Worth a look; the table loads.
    Warning,
    /// For information only.
    Info,
}

impl IssueSeverity {
    /// `error`, `warning` or `info`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

/// One problem found while reading or validating a chooser table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChooserIssue {
    /// Stable lowercase code (`chooser_bad_cell`, ...; see `book/agents/choosers.md`).
    pub code: &'static str,
    /// Error, warning or info.
    pub severity: IssueSeverity,
    /// Human-readable detail.
    pub message: String,
    /// The row, when the issue belongs to one.
    pub row: Option<String>,
    /// The column, when the issue belongs to one.
    pub column: Option<String>,
}

impl ChooserIssue {
    /// An error with no location.
    #[must_use]
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: IssueSeverity::Error,
            message: message.into(),
            row: None,
            column: None,
        }
    }

    /// Sets the severity.
    #[must_use]
    pub fn with_severity(mut self, severity: IssueSeverity) -> Self {
        self.severity = severity;
        self
    }

    /// Sets the row.
    #[must_use]
    pub fn with_row(mut self, row: &str) -> Self {
        self.row = Some(row.to_owned());
        self
    }

    /// Sets the column.
    #[must_use]
    pub fn with_column(mut self, column: &str) -> Self {
        self.column = Some(column.to_owned());
        self
    }

    /// Whether the issue is an error.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.severity == IssueSeverity::Error
    }
}

impl std::fmt::Display for ChooserIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.severity.name(), self.code)?;
        if let Some(row) = &self.row {
            write!(f, " row `{row}`")?;
        }
        if let Some(column) = &self.column {
            write!(f, " column `{column}`")?;
        }
        write!(f, ": {}", self.message)
    }
}

/// A table that did not load, or an edit that was refused: every error found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChooserError {
    /// The issues (at least one error).
    pub issues: Vec<ChooserIssue>,
}

impl ChooserError {
    pub(crate) fn one(issue: ChooserIssue) -> Self {
        Self { issues: vec![issue] }
    }

    /// The code of the first error (`chooser_bad_file` when there is none).
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.issues
            .iter()
            .find(|issue| issue.is_error())
            .map_or("chooser_bad_file", |issue| issue.code)
    }
}

impl std::fmt::Display for ChooserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let issues: Vec<String> = self.issues.iter().map(ToString::to_string).collect();
        f.write_str(&issues.join("; "))
    }
}

impl std::error::Error for ChooserError {}

/// The result of one evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct ChooserOutcome {
    /// The selected result (nested choosers already resolved).
    pub result: ResultEntry,
    /// The selected row of the evaluated table (`None` when the catch-all answered).
    pub row: Option<String>,
    /// Whether the catch-all answered.
    pub used_catch_all: bool,
    /// The outputs this outcome wrote, in write order (the selected row's emit columns, then the
    /// nested outcome's).
    pub outputs: Vec<(String, ChooserValue)>,
}

/// Resolves nested `Chooser(path)` results.
pub trait ChooserEnv {
    /// The table at asset path `path`.
    fn chooser(&self, path: &str) -> Option<&ChooserTable>;
}

/// A [`ChooserEnv`] with no tables: every `Chooser(..)` result is unresolved (evaluates as `None`).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoChoosers;

impl ChooserEnv for NoChoosers {
    fn chooser(&self, _path: &str) -> Option<&ChooserTable> {
        None
    }
}

impl ChooserEnv for BTreeMap<String, ChooserTable> {
    fn chooser(&self, path: &str) -> Option<&ChooserTable> {
        self.get(path)
    }
}

#[cfg(test)]
mod tests;
