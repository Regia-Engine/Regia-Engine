// SPDX-License-Identifier: MPL-2.0
//! The cooked (pak) form of a [`ChooserTable`] (spec 51 § 4.4, ADR-2012): a little-endian `u32`
//! version, then postcard of a flat list of tables, every table's nested `Inline` tables before it
//! and the top table last.
//!
//! Text types cells by their column (`ron::value::RawValue`), which postcard cannot do, so cells
//! are a typed enum here. The list is flat, and tag queries are post-order node lists, so a load
//! has no recursion a crafted payload could drive: every `Inline` names one earlier table, once.
//! A load then runs the structural checks text gets, plus what only the text parser refuses
//! (`NaN` in a value), so a damaged or edited pak gets the error its text form would.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{
    CHOOSER_COOKED_VERSION, CatchAll, Cell, ChooserError, ChooserIssue, ChooserTable, ChooserValue, Column,
    ColumnKind, Mode, Recency, ResultEntry, ResultType, Row, RowCells, ScalarKind, Schema, ValueKind,
};
use crate::tags::{MAX_QUERY_DEPTH, MAX_QUERY_NODES, Tag, TagContainer, TagQuery};

/// Deepest `Inline` nesting a cooked table may have. Evaluation stops at [`MAX_DEPTH`](super::MAX_DEPTH)
/// with the catch-all; this bounds the recursion of validation and of dropping a table, and sits
/// above anything the text parser's own nesting limit lets through.
pub const MAX_COOKED_NESTING: u32 = 64;

/// One post-order node of a tag query; a combinator takes the last `n` finished children.
#[derive(Serialize, Deserialize)]
enum CookedQueryNode {
    Has(String),
    HasExact(String),
    AllOf(u32),
    AnyOf(u32),
    NoneOf(u32),
}

#[derive(Serialize, Deserialize)]
enum CookedValue {
    Bool(bool),
    Int(i64),
    Float(f32),
    Enum(String),
    Name(String),
    Tags(Vec<String>),
    Asset(String),
}

#[derive(Serialize, Deserialize)]
enum CookedCell {
    Any,
    Bool(bool),
    IntRange(i64, i64),
    FloatRange(f32, f32),
    Is(String),
    IsNot(String),
    OneOf(Vec<String>),
    Kind(String),
    Name(String),
    Tags(Vec<CookedQueryNode>),
    Target(f32),
    Weight(f32),
    Value(CookedValue),
}

#[derive(Serialize, Deserialize)]
enum CookedResult {
    Asset(String),
    Value(CookedValue),
    Chooser(String),
    /// Index of the nested table in the file's list; it is before this table's own index.
    Inline(u32),
    Nothing,
}

#[derive(Serialize, Deserialize)]
struct CookedColumn {
    id: String,
    input: Option<String>,
    output: Option<String>,
    kind: ColumnKind,
    wrap: Option<(f64, f64)>,
    case_sensitive: bool,
    disabled: bool,
}

#[derive(Serialize, Deserialize)]
struct CookedRow {
    id: String,
    enabled: bool,
    /// `(column index, cell)`: an index, so a row carries no column id strings.
    cells: Vec<(u16, CookedCell)>,
    result: CookedResult,
    note: String,
}

#[derive(Serialize, Deserialize)]
struct CookedCatchAll {
    result: CookedResult,
    outputs: Vec<(String, CookedValue)>,
}

#[derive(Serialize, Deserialize)]
struct CookedTable {
    mode: Mode,
    result: ResultType,
    inputs: Vec<(String, ValueKind)>,
    outputs: Vec<(String, ValueKind)>,
    columns: Vec<CookedColumn>,
    rows: Vec<CookedRow>,
    catch_all: Option<CookedCatchAll>,
    near_best: Option<f32>,
    recency: Option<Recency>,
}

fn bad_file(message: impl Into<String>) -> ChooserError {
    ChooserError::one(ChooserIssue::error("chooser_bad_file", message))
}

fn too_deep() -> ChooserError {
    ChooserError::one(ChooserIssue::error(
        "chooser_depth_exceeded",
        format!("inline tables nest deeper than {MAX_COOKED_NESTING}"),
    ))
}

/// `query` as post-order nodes. Queries deeper or larger than the text form allows are refused.
fn query_nodes(query: &TagQuery, depth: usize, out: &mut Vec<CookedQueryNode>) -> Result<(), ChooserError> {
    if depth >= MAX_QUERY_DEPTH || out.len() >= MAX_QUERY_NODES {
        return Err(bad_file("a tag query exceeds the query limits"));
    }
    let children = |list: &[TagQuery], out: &mut Vec<CookedQueryNode>| -> Result<u32, ChooserError> {
        for child in list {
            query_nodes(child, depth + 1, out)?;
        }
        u32::try_from(list.len()).map_err(|_| bad_file("a tag query has too many children"))
    };
    let node = match query {
        TagQuery::Has(tag) => CookedQueryNode::Has(tag.name().to_owned()),
        TagQuery::HasExact(tag) => CookedQueryNode::HasExact(tag.name().to_owned()),
        TagQuery::AllOf(list) => CookedQueryNode::AllOf(children(list, out)?),
        TagQuery::AnyOf(list) => CookedQueryNode::AnyOf(children(list, out)?),
        TagQuery::NoneOf(list) => CookedQueryNode::NoneOf(children(list, out)?),
    };
    out.push(node);
    Ok(())
}

/// The query of post-order `nodes`, with the limits [`TagQuery::from_ron`] applies.
fn query_of(nodes: Vec<CookedQueryNode>) -> Result<TagQuery, ChooserError> {
    if nodes.len() > MAX_QUERY_NODES {
        return Err(bad_file("a cooked tag query has too many nodes"));
    }
    let tag = |name: &str| Tag::new(name).map_err(|e| bad_file(format!("cooked tag query: {e}")));
    // Each entry is a finished subtree and its depth.
    let mut stack: Vec<(TagQuery, usize)> = Vec::new();
    for node in nodes {
        let (arity, build): (u32, fn(Vec<TagQuery>) -> TagQuery) = match node {
            CookedQueryNode::Has(name) => {
                stack.push((TagQuery::Has(tag(&name)?), 1));
                continue;
            }
            CookedQueryNode::HasExact(name) => {
                stack.push((TagQuery::HasExact(tag(&name)?), 1));
                continue;
            }
            CookedQueryNode::AllOf(n) => (n, TagQuery::AllOf),
            CookedQueryNode::AnyOf(n) => (n, TagQuery::AnyOf),
            CookedQueryNode::NoneOf(n) => (n, TagQuery::NoneOf),
        };
        let at = usize::try_from(arity)
            .ok()
            .and_then(|n| stack.len().checked_sub(n))
            .ok_or_else(|| bad_file("a cooked tag query is malformed"))?;
        let mut depth = 1;
        let children: Vec<TagQuery> = stack
            .drain(at..)
            .map(|(child, d)| {
                depth = depth.max(d + 1);
                child
            })
            .collect();
        if depth > MAX_QUERY_DEPTH {
            return Err(bad_file("a cooked tag query is too deep"));
        }
        stack.push((build(children), depth));
    }
    match (stack.pop(), stack.is_empty()) {
        (Some((query, _)), true) => Ok(query),
        _ => Err(bad_file("a cooked tag query is malformed")),
    }
}

fn cooked_value(value: &ChooserValue) -> CookedValue {
    match value {
        ChooserValue::Bool(v) => CookedValue::Bool(*v),
        ChooserValue::Int(v) => CookedValue::Int(*v),
        ChooserValue::Float(v) => CookedValue::Float(*v),
        ChooserValue::Enum(v) => CookedValue::Enum(v.clone()),
        ChooserValue::Name(v) => CookedValue::Name(v.clone()),
        ChooserValue::Tags(v) => CookedValue::Tags(v.names().iter().map(|n| (*n).to_owned()).collect()),
        ChooserValue::Asset(v) => CookedValue::Asset(v.clone()),
    }
}

fn value_of(value: CookedValue) -> Result<ChooserValue, ChooserError> {
    Ok(match value {
        CookedValue::Bool(v) => ChooserValue::Bool(v),
        CookedValue::Int(v) => ChooserValue::Int(v),
        // Only the text parser refuses this; the structural checks do not look at values.
        CookedValue::Float(v) if v.is_nan() => {
            return Err(ChooserError::one(ChooserIssue::error(
                "chooser_bad_number",
                "NaN is not allowed",
            )));
        }
        CookedValue::Float(v) => ChooserValue::Float(v),
        CookedValue::Enum(v) => ChooserValue::Enum(v),
        CookedValue::Name(v) => ChooserValue::Name(v),
        CookedValue::Tags(names) => ChooserValue::Tags(
            TagContainer::from_names(names.iter().map(String::as_str))
                .map_err(|e| bad_file(format!("cooked tags: {e}")))?,
        ),
        CookedValue::Asset(v) => ChooserValue::Asset(v),
    })
}

fn cooked_cell(cell: &Cell) -> Result<CookedCell, ChooserError> {
    Ok(match cell {
        Cell::Any => CookedCell::Any,
        Cell::Bool(v) => CookedCell::Bool(*v),
        Cell::IntRange(a, b) => CookedCell::IntRange(*a, *b),
        Cell::FloatRange(a, b) => CookedCell::FloatRange(*a, *b),
        Cell::Is(v) => CookedCell::Is(v.clone()),
        Cell::IsNot(v) => CookedCell::IsNot(v.clone()),
        Cell::OneOf(v) => CookedCell::OneOf(v.clone()),
        Cell::Kind(v) => CookedCell::Kind(v.clone()),
        Cell::Name(v) => CookedCell::Name(v.clone()),
        Cell::Tags(query) => {
            let mut nodes = Vec::new();
            query_nodes(query, 0, &mut nodes)?;
            CookedCell::Tags(nodes)
        }
        Cell::Target(v) => CookedCell::Target(*v),
        Cell::Weight(v) => CookedCell::Weight(*v),
        Cell::Value(v) => CookedCell::Value(cooked_value(v)),
    })
}

fn cell_of(cell: CookedCell) -> Result<Cell, ChooserError> {
    Ok(match cell {
        CookedCell::Any => Cell::Any,
        CookedCell::Bool(v) => Cell::Bool(v),
        CookedCell::IntRange(a, b) => Cell::IntRange(a, b),
        CookedCell::FloatRange(a, b) => Cell::FloatRange(a, b),
        CookedCell::Is(v) => Cell::Is(v),
        CookedCell::IsNot(v) => Cell::IsNot(v),
        CookedCell::OneOf(v) => Cell::OneOf(v),
        CookedCell::Kind(v) => Cell::Kind(v),
        CookedCell::Name(v) => Cell::Name(v),
        CookedCell::Tags(nodes) => Cell::Tags(query_of(nodes)?),
        CookedCell::Target(v) => Cell::Target(v),
        CookedCell::Weight(v) => Cell::Weight(v),
        CookedCell::Value(v) => Cell::Value(value_of(v)?),
    })
}

/// Appends `table` to `tables` after its inline tables; returns its index.
fn flatten(table: &ChooserTable, depth: u32, tables: &mut Vec<CookedTable>) -> Result<u32, ChooserError> {
    if depth > MAX_COOKED_NESTING {
        return Err(too_deep());
    }
    let mut result = |entry: &ResultEntry| -> Result<CookedResult, ChooserError> {
        Ok(match entry {
            ResultEntry::Asset(path) => CookedResult::Asset(path.clone()),
            ResultEntry::Value(value) => CookedResult::Value(cooked_value(value)),
            ResultEntry::Chooser(path) => CookedResult::Chooser(path.clone()),
            ResultEntry::Inline(nested) => CookedResult::Inline(flatten(nested, depth + 1, tables)?),
            ResultEntry::Nothing => CookedResult::Nothing,
        })
    };
    let mut rows = Vec::with_capacity(table.rows.len());
    for row in &table.rows {
        let mut cells = Vec::with_capacity(row.cells.len());
        for (column_id, cell) in &row.cells {
            let index = table
                .columns
                .iter()
                .position(|c| &c.id == column_id)
                .and_then(|i| u16::try_from(i).ok())
                .ok_or_else(|| {
                    ChooserError::one(
                        ChooserIssue::error("chooser_bad_cell", format!("no column `{column_id}`"))
                            .with_row(&row.id)
                            .with_column(column_id),
                    )
                })?;
            cells.push((index, cooked_cell(cell)?));
        }
        rows.push(CookedRow {
            id: row.id.clone(),
            enabled: row.enabled,
            cells,
            result: result(&row.result)?,
            note: row.note.clone(),
        });
    }
    let catch_all = match &table.catch_all {
        Some(catch_all) => Some(CookedCatchAll {
            result: result(&catch_all.result)?,
            outputs: catch_all
                .outputs
                .iter()
                .map(|(name, value)| (name.clone(), cooked_value(value)))
                .collect(),
        }),
        None => None,
    };
    let kinds = |map: &BTreeMap<String, ValueKind>| -> Vec<(String, ValueKind)> {
        map.iter().map(|(n, k)| (n.clone(), k.clone())).collect()
    };
    tables.push(CookedTable {
        mode: table.mode,
        result: table.result_type.clone(),
        inputs: kinds(&table.schema.inputs),
        outputs: kinds(&table.schema.outputs),
        columns: table
            .columns
            .iter()
            .map(|c| CookedColumn {
                id: c.id.clone(),
                input: c.input.clone(),
                output: c.output.clone(),
                kind: c.kind.clone(),
                wrap: c.wrap,
                case_sensitive: c.case_sensitive,
                disabled: c.disabled,
            })
            .collect(),
        rows,
        catch_all,
        near_best: table.near_best,
        recency: table.recency,
    });
    u32::try_from(tables.len() - 1).map_err(|_| bad_file("too many inline tables"))
}

/// A table already built, waiting for the `Inline` that takes it, with its nesting height.
type Finished = Option<(ChooserTable, u32)>;

fn entry_of(
    result: CookedResult,
    finished: &mut [Finished],
    height: &mut u32,
) -> Result<ResultEntry, ChooserError> {
    Ok(match result {
        CookedResult::Asset(path) => ResultEntry::Asset(path),
        CookedResult::Value(value) => ResultEntry::Value(value_of(value)?),
        CookedResult::Chooser(path) => ResultEntry::Chooser(path),
        CookedResult::Nothing => ResultEntry::Nothing,
        CookedResult::Inline(index) => {
            // Out of range covers a table naming itself or a later one; `take` covers a second
            // `Inline` of one table (shared tables would multiply on load).
            let (nested, nested_height) = usize::try_from(index)
                .ok()
                .and_then(|i| finished.get_mut(i))
                .and_then(Option::take)
                .ok_or_else(|| bad_file(format!("inline table {index} is dangling or used twice")))?;
            *height = (*height).max(nested_height + 1);
            if *height > MAX_COOKED_NESTING {
                return Err(too_deep());
            }
            ResultEntry::Inline(Box::new(nested))
        }
    })
}

fn table_of(cooked: CookedTable, finished: &mut [Finished]) -> Result<(ChooserTable, u32), ChooserError> {
    let mut height = 0;
    let columns: Vec<Column> = cooked
        .columns
        .into_iter()
        .map(|c| Column {
            id: c.id,
            input: c.input,
            output: c.output,
            kind: c.kind,
            wrap: c.wrap,
            case_sensitive: c.case_sensitive,
            disabled: c.disabled,
        })
        .collect();
    let mut rows = Vec::with_capacity(cooked.rows.len());
    for row in cooked.rows {
        let mut cells = Vec::with_capacity(row.cells.len());
        for (index, cell) in row.cells {
            let column = columns
                .get(usize::from(index))
                .ok_or_else(|| bad_file(format!("row `{}` names column {index}", row.id)))?;
            cells.push((column.id.clone(), cell_of(cell)?));
        }
        let cells = RowCells::from_unsorted(cells)
            .map_err(|column| bad_file(format!("row `{}` has two cells for `{column}`", row.id)))?;
        rows.push(Row {
            id: row.id,
            enabled: row.enabled,
            cells,
            result: entry_of(row.result, finished, &mut height)?,
            note: row.note,
        });
    }
    let catch_all = match cooked.catch_all {
        Some(catch_all) => {
            let result = entry_of(catch_all.result, finished, &mut height)?;
            let mut outputs = BTreeMap::new();
            for (name, value) in catch_all.outputs {
                outputs.insert(name, value_of(value)?);
            }
            Some(CatchAll { result, outputs })
        }
        None => None,
    };
    let table = ChooserTable {
        schema: Schema {
            inputs: cooked.inputs.into_iter().collect(),
            outputs: cooked.outputs.into_iter().collect(),
        },
        mode: cooked.mode,
        result_type: cooked.result,
        columns,
        rows,
        catch_all,
        near_best: cooked.near_best,
        recency: cooked.recency,
    };
    Ok((table, height))
}

/// The serde shape fingerprint of the cooked form (hashed by `cargo xtask check-formats`): every
/// struct's field list and every enum's variant spellings.
#[must_use]
pub fn chooser_cooked_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<CookedTable>(),
        serde_field_names::<CookedColumn>(),
        serde_field_names::<CookedRow>(),
        serde_field_names::<CookedCatchAll>(),
        serde_field_names::<Recency>(),
        serde_variant_names::<Mode>(),
        serde_variant_names::<ResultType>(),
        serde_variant_names::<ScalarKind>(),
        serde_variant_names::<ValueKind>(),
        serde_variant_names::<ColumnKind>(),
        serde_variant_names::<CookedCell>(),
        serde_variant_names::<CookedValue>(),
        serde_variant_names::<CookedResult>(),
        serde_variant_names::<CookedQueryNode>(),
    ])
}

impl ChooserTable {
    /// The form `regia cook` writes into a pak. [`from_cooked`](Self::from_cooked) of these bytes
    /// is this table; the bytes are the same for the same table.
    ///
    /// # Errors
    ///
    /// `chooser_bad_cell` (a cell of no column), `chooser_depth_exceeded` (inline tables nested
    /// deeper than [`MAX_COOKED_NESTING`]), `chooser_bad_file` (a query past the query limits).
    pub fn to_cooked(&self) -> Result<Vec<u8>, ChooserError> {
        let mut tables = Vec::new();
        flatten(self, 0, &mut tables)?;
        let out = CHOOSER_COOKED_VERSION.to_le_bytes().to_vec();
        postcard::to_extend(&tables, out).map_err(|e| bad_file(e.to_string()))
    }

    /// Loads the cooked form and validates it as [`from_ron_str`](Self::from_ron_str) does. Nested
    /// `Chooser(..)` paths are checked later by [`validate`](Self::validate).
    ///
    /// # Errors
    ///
    /// `chooser_unsupported_version` (a version this build does not read), `chooser_bad_file`
    /// (damaged or truncated bytes, bytes after the table, an `Inline` that is dangling or used
    /// twice), `chooser_depth_exceeded`, or every structural error.
    pub fn from_cooked(bytes: &[u8]) -> Result<Self, ChooserError> {
        let _span = tracing::debug_span!(target: "regia_framework", "chooser_load_cooked").entered();
        let Some((version, body)) = bytes.split_first_chunk::<4>() else {
            return Err(bad_file("cooked chooser is shorter than its version"));
        };
        let version = u32::from_le_bytes(*version);
        if version == 0 || version > CHOOSER_COOKED_VERSION {
            return Err(ChooserError::one(ChooserIssue::error(
                "chooser_unsupported_version",
                format!(
                    "cooked version {version}; this engine reads cooked choosers up to {CHOOSER_COOKED_VERSION}"
                ),
            )));
        }
        let (tables, rest) = postcard::take_from_bytes::<Vec<CookedTable>>(body)
            .map_err(|e| bad_file(format!("cooked chooser: {e}")))?;
        if !rest.is_empty() {
            return Err(bad_file(format!("{} bytes after the cooked chooser", rest.len())));
        }
        let mut finished: Vec<Finished> = Vec::with_capacity(tables.len());
        for cooked in tables {
            let built = table_of(cooked, &mut finished)?;
            finished.push(Some(built));
        }
        let Some(Some((table, _))) = finished.pop() else {
            return Err(bad_file("a cooked chooser holds no table"));
        };
        if finished.iter().any(Option::is_some) {
            return Err(bad_file("a cooked chooser holds a table nothing uses"));
        }
        let issues = table.validate_structure();
        if issues.iter().any(ChooserIssue::is_error) {
            return Err(ChooserError { issues });
        }
        Ok(table)
    }

    /// Every path a `Chooser(..)` result of this table or its inline tables names, sorted.
    #[must_use]
    pub fn nested_choosers(&self) -> Vec<String> {
        let mut out = BTreeSet::new();
        self.visit_entries(&mut |entry| {
            if let ResultEntry::Chooser(path) = entry {
                out.insert(path.clone());
            }
        });
        out.into_iter().collect()
    }

    /// Every asset path this table or its inline tables name: `Asset(..)` and `Chooser(..)`
    /// results, `Is(..)` cells of `Asset` columns and `Asset` values of cells, results and
    /// catch-all outputs; sorted, without duplicates. These are its cooked dependencies.
    #[must_use]
    pub fn asset_references(&self) -> Vec<String> {
        let mut out = BTreeSet::new();
        self.collect_references(&mut out);
        out.into_iter().collect()
    }

    fn collect_references(&self, out: &mut BTreeSet<String>) {
        let note_value = |value: &ChooserValue, out: &mut BTreeSet<String>| {
            if let ChooserValue::Asset(path) = value {
                out.insert(path.clone());
            }
        };
        for row in &self.rows {
            for (column_id, cell) in &row.cells {
                match cell {
                    Cell::Is(path)
                        if self
                            .columns
                            .iter()
                            .any(|c| &c.id == column_id && c.kind == ColumnKind::Asset) =>
                    {
                        out.insert(path.clone());
                    }
                    Cell::Value(value) => note_value(value, out),
                    _ => {}
                }
            }
        }
        if let Some(catch_all) = &self.catch_all {
            for value in catch_all.outputs.values() {
                note_value(value, out);
            }
        }
        let mut nested = Vec::new();
        let rows = self.rows.iter().map(|row| &row.result);
        for entry in rows.chain(self.catch_all.iter().map(|c| &c.result)) {
            match entry {
                ResultEntry::Asset(path) | ResultEntry::Chooser(path) => {
                    out.insert(path.clone());
                }
                ResultEntry::Value(value) => note_value(value, out),
                ResultEntry::Inline(table) => nested.push(table),
                ResultEntry::Nothing => {}
            }
        }
        for table in nested {
            table.collect_references(out);
        }
    }

    fn visit_entries(&self, visit: &mut impl FnMut(&ResultEntry)) {
        let rows = self.rows.iter().map(|row| &row.result);
        for entry in rows.chain(self.catch_all.iter().map(|c| &c.result)) {
            visit(entry);
            if let ResultEntry::Inline(table) = entry {
                table.visit_entries(visit);
            }
        }
    }

    /// The registry's `result=` tag of this table: the asset kind of an `Asset(..)` result type,
    /// the lowercase value kind of a `Value(..)` one, `output_only` otherwise.
    #[must_use]
    pub fn result_tag(&self) -> String {
        match &self.result_type {
            ResultType::Asset(kind) => kind.clone(),
            ResultType::Value(kind) => kind.value_kind().name().to_owned(),
            ResultType::OutputOnly => "output_only".to_owned(),
        }
    }
}

#[cfg(test)]
pub(super) mod hostile {
    //! Builders for payloads no encoder writes.

    use super::*;

    /// A file whose top table has no rows and which ends with one row per `Inline` index given.
    pub(in super::super) fn inline_payload(tables: usize, inline: &[u32]) -> Vec<u8> {
        let blank = || CookedTable {
            mode: Mode::First,
            result: ResultType::OutputOnly,
            inputs: Vec::new(),
            outputs: Vec::new(),
            columns: Vec::new(),
            rows: Vec::new(),
            catch_all: None,
            near_best: None,
            recency: None,
        };
        let mut list: Vec<CookedTable> = (0..tables).map(|_| blank()).collect();
        if let Some(last) = list.last_mut() {
            last.rows = inline
                .iter()
                .enumerate()
                .map(|(i, index)| CookedRow {
                    id: format!("r{i}"),
                    enabled: true,
                    cells: Vec::new(),
                    result: CookedResult::Inline(*index),
                    note: String::new(),
                })
                .collect();
        }
        encode(&list)
    }

    /// `depth` tables, each the only `Inline` of the next.
    pub(in super::super) fn nested_payload(depth: u32) -> Vec<u8> {
        let mut list = Vec::new();
        for i in 0..depth {
            let mut bytes_table = CookedTable {
                mode: Mode::First,
                result: ResultType::OutputOnly,
                inputs: Vec::new(),
                outputs: Vec::new(),
                columns: Vec::new(),
                rows: Vec::new(),
                catch_all: None,
                near_best: None,
                recency: None,
            };
            if i > 0 {
                bytes_table.rows.push(CookedRow {
                    id: "r".to_owned(),
                    enabled: true,
                    cells: Vec::new(),
                    result: CookedResult::Inline(i - 1),
                    note: String::new(),
                });
            }
            list.push(bytes_table);
        }
        encode(&list)
    }

    /// One table with one `Value(..)`-result row holding a NaN.
    pub(in super::super) fn nan_value_payload() -> Vec<u8> {
        let table = CookedTable {
            mode: Mode::First,
            result: ResultType::Value(ScalarKind::Float),
            inputs: Vec::new(),
            outputs: Vec::new(),
            columns: Vec::new(),
            rows: vec![CookedRow {
                id: "r".to_owned(),
                enabled: true,
                cells: Vec::new(),
                result: CookedResult::Value(CookedValue::Float(f32::NAN)),
                note: String::new(),
            }],
            catch_all: None,
            near_best: None,
            recency: None,
        };
        encode(&[table])
    }

    /// One table with one row whose only cell is `cell` for `column`.
    pub(in super::super) fn row_cell_payload(column: u16, nodes: Vec<(bool, u32)>) -> Vec<u8> {
        // `(true, _)` is a leaf `Has("A")`; `(false, n)` an `AllOf(n)`.
        let nodes = nodes
            .into_iter()
            .map(|(leaf, n)| {
                if leaf {
                    CookedQueryNode::Has("A".to_owned())
                } else {
                    CookedQueryNode::AllOf(n)
                }
            })
            .collect();
        let table = CookedTable {
            mode: Mode::First,
            result: ResultType::OutputOnly,
            inputs: vec![("t".to_owned(), ValueKind::Tags)],
            outputs: Vec::new(),
            columns: vec![CookedColumn {
                id: "c".to_owned(),
                input: Some("t".to_owned()),
                output: None,
                kind: ColumnKind::Tags,
                wrap: None,
                case_sensitive: true,
                disabled: false,
            }],
            rows: vec![CookedRow {
                id: "r".to_owned(),
                enabled: true,
                cells: vec![(column, CookedCell::Tags(nodes))],
                result: CookedResult::Nothing,
                note: String::new(),
            }],
            catch_all: None,
            near_best: None,
            recency: None,
        };
        encode(&[table])
    }

    fn encode(tables: &[CookedTable]) -> Vec<u8> {
        let out = CHOOSER_COOKED_VERSION.to_le_bytes().to_vec();
        postcard::to_extend(&tables, out).expect("encode")
    }
}
