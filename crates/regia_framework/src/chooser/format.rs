// SPDX-License-Identifier: MPL-2.0
//! The `.chooser.ron` form (format `chooser` v1): reading through serde with each cell kept as raw
//! RON text and typed by its column's kind, and the canonical writer.

use std::collections::BTreeMap;
use std::fmt;

use regia_core::version;
use ron::value::RawValue;
use serde::{Deserialize, Deserializer};

use super::{
    CHOOSER_FORMAT_VERSION, CatchAll, Cell, ChooserError, ChooserIssue, ChooserTable, ChooserValue, Column,
    ColumnKind, Mode, Recency, ResultEntry, ResultType, Row, ScalarKind, Schema, ValueKind,
};
use crate::tags::{TagContainer, TagQuery};

/// A table body: the whole file, or an `Inline(..)` result (where `format_version` is absent).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TableRepr {
    #[serde(default, deserialize_with = "bare_some")]
    format_version: Option<u32>,
    #[serde(default)]
    mode: Mode,
    result: ResultType,
    #[serde(default)]
    inputs: BTreeMap<String, ValueKind>,
    #[serde(default)]
    outputs: BTreeMap<String, ValueKind>,
    #[serde(default)]
    columns: Vec<ColumnRepr>,
    #[serde(default)]
    rows: Vec<RowRepr>,
    #[serde(default)]
    catch_all: Option<CatchAllRepr>,
    #[serde(default)]
    near_best: Option<f32>,
    #[serde(default)]
    recency: Option<Recency>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ColumnRepr {
    id: String,
    #[serde(default, deserialize_with = "bare_some")]
    input: Option<String>,
    #[serde(default, deserialize_with = "bare_some")]
    output: Option<String>,
    kind: ColumnKind,
    #[serde(default)]
    wrap: Option<(f64, f64)>,
    #[serde(default = "yes")]
    case_sensitive: bool,
    #[serde(default)]
    disabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RowRepr {
    id: String,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    cells: BTreeMap<String, Box<RawValue>>,
    #[serde(default)]
    result: ResultRepr,
    #[serde(default)]
    note: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatchAllRepr {
    #[serde(default)]
    result: ResultRepr,
    #[serde(default)]
    outputs: BTreeMap<String, Box<RawValue>>,
}

#[derive(Deserialize, Default)]
enum ResultRepr {
    Asset(String),
    Value(Box<RawValue>),
    Chooser(String),
    Inline(Box<TableRepr>),
    #[default]
    Nothing,
}

#[derive(Deserialize)]
enum EnumCellRepr {
    Any,
    Is(String),
    IsNot(String),
    OneOf(Vec<String>),
}

#[derive(Deserialize)]
enum AssetCellRepr {
    Any,
    Is(String),
    Kind(String),
}

#[derive(Deserialize)]
enum BoolCellRepr {
    Any,
    True,
    False,
}

fn yes() -> bool {
    true
}

/// `input: "armed"` (no `Some(..)` wrapper) for an optional field.
fn bare_some<'de, D: Deserializer<'de>, T: Deserialize<'de>>(deserializer: D) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

/// The `.chooser.ron` migration chain: v1 is the first version, so there are no steps.
#[must_use]
pub fn chooser_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape fingerprint of `.chooser.ron` (hashed by `cargo xtask check-formats`): every
/// struct's field list and every enum's variant spellings.
#[must_use]
pub fn chooser_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<TableRepr>(),
        serde_field_names::<ColumnRepr>(),
        serde_field_names::<RowRepr>(),
        serde_field_names::<CatchAllRepr>(),
        serde_field_names::<Recency>(),
        serde_variant_names::<Mode>(),
        serde_variant_names::<ResultType>(),
        serde_variant_names::<ScalarKind>(),
        serde_variant_names::<ValueKind>(),
        serde_variant_names::<ColumnKind>(),
        serde_variant_names::<ResultRepr>(),
        serde_variant_names::<EnumCellRepr>(),
        serde_variant_names::<AssetCellRepr>(),
        serde_variant_names::<BoolCellRepr>(),
    ])
}

fn is_any(text: &str) -> bool {
    text.trim() == "Any"
}

/// Parses a value of `kind` from RON text: `true`, `3`, `1.5`, `"name"`, `["A.B", "C"]`.
///
/// # Errors
///
/// A message when the text is not a value of `kind` (an `Enum` value must be a declared variant).
pub fn parse_value(kind: &ValueKind, text: &str) -> Result<ChooserValue, String> {
    let text = text.trim();
    let value = match kind {
        ValueKind::Bool => ChooserValue::Bool(ron::from_str(text).map_err(|e| e.to_string())?),
        ValueKind::Int => ChooserValue::Int(ron::from_str(text).map_err(|e| e.to_string())?),
        ValueKind::Float => ChooserValue::Float(ron::from_str(text).map_err(|e| e.to_string())?),
        ValueKind::Enum(_) => ChooserValue::Enum(ron::from_str(text).map_err(|e| e.to_string())?),
        ValueKind::Name => ChooserValue::Name(ron::from_str(text).map_err(|e| e.to_string())?),
        ValueKind::Asset => ChooserValue::Asset(ron::from_str(text).map_err(|e| e.to_string())?),
        ValueKind::Tags => {
            let names: Vec<String> = ron::from_str(text).map_err(|e| e.to_string())?;
            ChooserValue::Tags(
                TagContainer::from_names(names.iter().map(String::as_str)).map_err(|e| e.to_string())?,
            )
        }
    };
    if !kind.admits(&value) {
        return Err(format!("`{text}` is not one of the declared variants"));
    }
    if matches!(value, ChooserValue::Float(v) if v.is_nan()) {
        return Err(NAN_REFUSED.to_owned());
    }
    Ok(value)
}

/// The message [`parse_value`] gives a NaN float. The loader reports it as `chooser_bad_number`
/// (spec 51 § 2.5: NaN anywhere), whatever the place the value sits in.
const NAN_REFUSED: &str = "NaN is not allowed";

/// `chooser_bad_number` for a NaN refusal from [`parse_value`], else `fallback`.
fn value_error_code(message: &str, fallback: &'static str) -> &'static str {
    if message == NAN_REFUSED {
        "chooser_bad_number"
    } else {
        fallback
    }
}

/// The RON text of a value (the form [`parse_value`] reads).
#[must_use]
pub fn value_ron(value: &ChooserValue) -> String {
    match value {
        ChooserValue::Bool(b) => b.to_string(),
        ChooserValue::Int(i) => i.to_string(),
        ChooserValue::Float(f) => format!("{f:?}"),
        ChooserValue::Enum(s) | ChooserValue::Name(s) | ChooserValue::Asset(s) => quoted(s),
        ChooserValue::Tags(tags) => {
            let names: Vec<String> = tags.names().iter().map(|n| quoted(n)).collect();
            format!("[{}]", names.join(", "))
        }
    }
}

/// The output kind an emit column writes.
fn emit_kind(kind: &ColumnKind, schema: &Schema, output: Option<&str>) -> Option<ValueKind> {
    let declared = output.and_then(|o| schema.outputs.get(o));
    match kind {
        ColumnKind::SetBool => Some(ValueKind::Bool),
        ColumnKind::SetInt => Some(ValueKind::Int),
        ColumnKind::SetFloat => Some(ValueKind::Float),
        ColumnKind::SetEnum => Some(declared.cloned().unwrap_or(ValueKind::Enum(Vec::new()))),
        ColumnKind::SetName => Some(ValueKind::Name),
        ColumnKind::SetTags => Some(ValueKind::Tags),
        ColumnKind::SetAsset => Some(ValueKind::Asset),
        _ => None,
    }
}

/// Parses one cell's RON text for `column`.
///
/// # Errors
///
/// A message when the text is not a cell of the column's kind.
pub fn parse_cell(column: &Column, schema: &Schema, text: &str) -> Result<Cell, String> {
    let text = text.trim();
    let err = |e: ron::error::SpannedError| e.to_string();
    Ok(match &column.kind {
        ColumnKind::Bool => match ron::from_str::<BoolCellRepr>(text).map_err(err)? {
            BoolCellRepr::Any => Cell::Any,
            BoolCellRepr::True => Cell::Bool(true),
            BoolCellRepr::False => Cell::Bool(false),
        },
        ColumnKind::IntRange if is_any(text) => Cell::Any,
        ColumnKind::IntRange => {
            let (a, b): (i64, i64) = ron::from_str(text).map_err(err)?;
            Cell::IntRange(a, b)
        }
        ColumnKind::FloatRange if is_any(text) => Cell::Any,
        ColumnKind::FloatRange => {
            let (a, b): (f32, f32) = ron::from_str(text).map_err(err)?;
            Cell::FloatRange(a, b)
        }
        ColumnKind::Enum => match ron::from_str::<EnumCellRepr>(text).map_err(err)? {
            EnumCellRepr::Any => Cell::Any,
            EnumCellRepr::Is(v) => Cell::Is(v),
            EnumCellRepr::IsNot(v) => Cell::IsNot(v),
            EnumCellRepr::OneOf(v) => Cell::OneOf(v),
        },
        ColumnKind::Asset => match ron::from_str::<AssetCellRepr>(text).map_err(err)? {
            AssetCellRepr::Any => Cell::Any,
            AssetCellRepr::Is(v) => Cell::Is(v),
            AssetCellRepr::Kind(v) => Cell::Kind(v),
        },
        ColumnKind::Name if is_any(text) => Cell::Any,
        ColumnKind::Name => Cell::Name(ron::from_str(text).map_err(err)?),
        ColumnKind::Tags if is_any(text) => Cell::Any,
        ColumnKind::Tags => Cell::Tags(TagQuery::from_ron(text).map_err(|e| e.to_string())?),
        ColumnKind::FloatDistance { .. } if is_any(text) => Cell::Any,
        ColumnKind::FloatDistance { .. } => Cell::Target(ron::from_str(text).map_err(err)?),
        ColumnKind::Weight => Cell::Weight(ron::from_str(text).map_err(err)?),
        kind => {
            let Some(value_kind) = emit_kind(kind, schema, column.output.as_deref()) else {
                return Err(format!("column kind {} takes no cells", kind.ron_name()));
            };
            if matches!(value_kind, ValueKind::Enum(ref v) if v.is_empty()) {
                Cell::Value(ChooserValue::Enum(ron::from_str(text).map_err(err)?))
            } else {
                Cell::Value(parse_value(&value_kind, text)?)
            }
        }
    })
}

/// The RON text of a cell (the form [`parse_cell`] reads).
#[must_use]
pub fn cell_ron(cell: &Cell) -> String {
    match cell {
        Cell::Any => "Any".to_owned(),
        Cell::Bool(true) => "True".to_owned(),
        Cell::Bool(false) => "False".to_owned(),
        Cell::IntRange(a, b) => format!("({a}, {b})"),
        Cell::FloatRange(a, b) => format!("({a:?}, {b:?})"),
        Cell::Is(v) => format!("Is({})", quoted(v)),
        Cell::IsNot(v) => format!("IsNot({})", quoted(v)),
        Cell::OneOf(v) => {
            let items: Vec<String> = v.iter().map(|s| quoted(s)).collect();
            format!("OneOf([{}])", items.join(", "))
        }
        Cell::Kind(v) => format!("Kind({})", quoted(v)),
        Cell::Name(v) => quoted(v),
        Cell::Tags(q) => q.to_ron().unwrap_or_else(|_| "Any".to_owned()),
        Cell::Target(v) | Cell::Weight(v) => format!("{v:?}"),
        Cell::Value(v) => value_ron(v),
    }
}

/// Parses a result entry's RON text (`Asset("anim/x.clip.ron")`, `Value(1.5)`,
/// `Chooser("a.chooser.ron")`, `Inline((..))`, `Nothing`) for a table of `result_type`.
///
/// # Errors
///
/// `chooser_bad_file` for text that is not a result entry, or the entry's typing issues.
pub fn parse_result(result_type: &ResultType, text: &str) -> Result<ResultEntry, Vec<ChooserIssue>> {
    let repr: ResultRepr =
        ron::from_str(text).map_err(|e| vec![ChooserIssue::error("chooser_bad_file", e.to_string())])?;
    let mut builder = Builder { issues: Vec::new() };
    let entry = builder.result(result_type, repr, "result");
    if builder.issues.is_empty() {
        Ok(entry)
    } else {
        Err(builder.issues)
    }
}

/// Parses a column kind's RON text (`Bool`, `FloatDistance(max_distance: 10.0)`, `SetFloat`).
///
/// # Errors
///
/// The RON error text.
pub fn parse_column_kind(text: &str) -> Result<ColumnKind, String> {
    ron::from_str(text).map_err(|e| e.to_string())
}

/// The RON text of a result entry (the form [`parse_result`] reads).
#[must_use]
pub fn result_ron(result: &ResultEntry) -> String {
    let mut out = String::new();
    write_result(&mut out, result, 0);
    out
}

struct Builder {
    issues: Vec<ChooserIssue>,
}

impl Builder {
    fn bad_cell(&mut self, row: &str, column: &str, message: String) {
        self.issues.push(
            ChooserIssue::error("chooser_bad_cell", message)
                .with_row(row)
                .with_column(column),
        );
    }

    /// A value [`parse_value`] refused: `chooser_bad_number` for NaN, else `chooser_bad_cell`.
    fn bad_value_cell(&mut self, row: &str, column: &str, raw: &str, message: String) {
        let code = value_error_code(&message, "chooser_bad_cell");
        self.issues.push(
            ChooserIssue::error(code, format!("`{}`: {message}", raw.trim()))
                .with_row(row)
                .with_column(column),
        );
    }

    fn table(&mut self, repr: TableRepr) -> ChooserTable {
        let schema = Schema {
            inputs: repr.inputs,
            outputs: repr.outputs,
        };
        let columns: Vec<Column> = repr
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
        let mut rows = Vec::with_capacity(repr.rows.len());
        for row in repr.rows {
            let mut cells = Vec::with_capacity(row.cells.len());
            for (column_id, raw) in &row.cells {
                let Some(column) = columns.iter().find(|c| &c.id == column_id) else {
                    self.bad_cell(&row.id, column_id, format!("no column `{column_id}`"));
                    continue;
                };
                match parse_cell(column, &schema, raw.get_ron()) {
                    Ok(cell) => cells.push((column_id.clone(), cell)),
                    Err(message) if message == NAN_REFUSED => {
                        self.bad_value_cell(&row.id, column_id, raw.get_ron(), message);
                    }
                    Err(message) => self.bad_cell(
                        &row.id,
                        column_id,
                        format!(
                            "`{}` is not a {} cell: {message}",
                            raw.get_ron().trim(),
                            column.kind.ron_name()
                        ),
                    ),
                }
            }
            let result = self.result(&repr.result, row.result, &row.id);
            rows.push(Row {
                id: row.id,
                enabled: row.enabled,
                cells: cells.into_iter().collect(),
                result,
                note: row.note,
            });
        }
        let catch_all = repr.catch_all.map(|c| {
            let result = self.result(&repr.result, c.result, "catch_all");
            let mut outputs = BTreeMap::new();
            for (name, raw) in &c.outputs {
                match schema.outputs.get(name) {
                    Some(kind) => match parse_value(kind, raw.get_ron()) {
                        Ok(value) => {
                            outputs.insert(name.clone(), value);
                        }
                        Err(message) => self.bad_value_cell("catch_all", name, raw.get_ron(), message),
                    },
                    None => self.issues.push(
                        ChooserIssue::error(
                            "chooser_unknown_input",
                            format!("catch_all writes undeclared output `{name}`"),
                        )
                        .with_row("catch_all")
                        .with_column(name),
                    ),
                }
            }
            CatchAll { result, outputs }
        });
        ChooserTable {
            schema,
            mode: repr.mode,
            result_type: repr.result,
            columns,
            rows,
            catch_all,
            near_best: repr.near_best,
            recency: repr.recency,
        }
    }

    fn result(&mut self, result_type: &ResultType, repr: ResultRepr, row: &str) -> ResultEntry {
        match repr {
            ResultRepr::Asset(path) => ResultEntry::Asset(path),
            ResultRepr::Chooser(path) => ResultEntry::Chooser(path),
            ResultRepr::Nothing => ResultEntry::Nothing,
            ResultRepr::Inline(table) => ResultEntry::Inline(Box::new(self.table(*table))),
            ResultRepr::Value(raw) => {
                let kind = match result_type {
                    ResultType::Value(kind) => kind.value_kind(),
                    _ => {
                        self.issues.push(
                            ChooserIssue::error(
                                "chooser_result_type_mismatch",
                                "a `Value(..)` result in a table whose result type is not `Value(..)`",
                            )
                            .with_row(row),
                        );
                        return ResultEntry::Nothing;
                    }
                };
                match parse_value(&kind, raw.get_ron()) {
                    Ok(value) => ResultEntry::Value(value),
                    Err(message) => {
                        self.issues.push(
                            ChooserIssue::error(
                                value_error_code(&message, "chooser_result_type_mismatch"),
                                format!("result value: {message}"),
                            )
                            .with_row(row),
                        );
                        ResultEntry::Nothing
                    }
                }
            }
        }
    }
}

impl ChooserTable {
    /// Parses a `.chooser.ron`: version check, shape, cells typed by their columns, then the
    /// structural checks of [`validate_structure`](Self::validate_structure). Nested `Chooser(..)`
    /// paths are checked later by [`validate`](Self::validate) against a [`ChooserEnv`](super::ChooserEnv).
    ///
    /// # Errors
    ///
    /// `chooser_bad_file` (syntax, shape, unknown field), `chooser_unsupported_version` (newer
    /// than [`CHOOSER_FORMAT_VERSION`], or 0), or every structural error.
    pub fn from_ron_str(text: &str) -> Result<Self, ChooserError> {
        let _span = tracing::debug_span!(target: "regia_framework", "chooser_load").entered();
        let unsupported = |found: u32| {
            ChooserError::one(ChooserIssue::error(
                "chooser_unsupported_version",
                format!("format_version {found}; this engine reads choosers up to {CHOOSER_FORMAT_VERSION}"),
            ))
        };
        // One parse on the happy path; the version is peeked only to explain a failure (a newer
        // file whose body no longer matches reports its version, not a shape error).
        let repr: TableRepr = match ron::from_str(text) {
            Ok(repr) => repr,
            Err(error) => {
                return Err(match version::peek_ron_version(text) {
                    Ok(found) if found == 0 || found > CHOOSER_FORMAT_VERSION => unsupported(found),
                    _ => ChooserError::one(ChooserIssue::error("chooser_bad_file", error.to_string())),
                });
            }
        };
        match repr.format_version {
            Some(found) if found == 0 || found > CHOOSER_FORMAT_VERSION => return Err(unsupported(found)),
            Some(_) => {}
            None => {
                return Err(ChooserError::one(ChooserIssue::error(
                    "chooser_bad_file",
                    "file has no `format_version` field",
                )));
            }
        }
        tracing::debug!(target: "regia_framework", format_version = ?repr.format_version, "chooser read");
        let mut builder = Builder { issues: Vec::new() };
        let table = builder.table(repr);
        let mut issues = builder.issues;
        issues.extend(table.validate_structure());
        if issues.iter().any(ChooserIssue::is_error) {
            return Err(ChooserError { issues });
        }
        Ok(table)
    }

    /// Writes the canonical `.chooser.ron` text (rows and columns in table order, maps sorted by
    /// key); ends with a newline. Writing a table read from this text gives the same bytes.
    #[must_use]
    pub fn to_ron_string(&self) -> String {
        let mut out = String::new();
        self.write_body(&mut out, 0, true);
        out.push('\n');
        out
    }

    fn write_body(&self, out: &mut String, depth: usize, top: bool) {
        let pad = "    ".repeat(depth + 1);
        let end = "    ".repeat(depth);
        out.push_str("(\n");
        if top {
            out.push_str(&format!("{pad}format_version: {CHOOSER_FORMAT_VERSION},\n"));
        }
        out.push_str(&format!("{pad}mode: {},\n", ron_enum(&self.mode)));
        out.push_str(&format!("{pad}result: {},\n", ron_enum(&self.result_type)));
        out.push_str(&format!("{pad}inputs: {},\n", kinds_ron(&self.schema.inputs)));
        out.push_str(&format!("{pad}outputs: {},\n", kinds_ron(&self.schema.outputs)));
        out.push_str(&format!("{pad}columns: [\n"));
        for column in &self.columns {
            out.push_str(&format!("{pad}    {},\n", column_ron(column)));
        }
        out.push_str(&format!("{pad}],\n{pad}rows: [\n"));
        for row in &self.rows {
            out.push_str(&format!("{pad}    (id: {}, ", quoted(&row.id)));
            if !row.enabled {
                out.push_str("enabled: false, ");
            }
            let cells: Vec<String> = row
                .cells
                .iter()
                .map(|(column, cell)| format!("{}: {}", quoted(column), cell_ron(cell)))
                .collect();
            out.push_str(&format!("cells: {{{}}}, result: ", cells.join(", ")));
            write_result(out, &row.result, depth + 2);
            if !row.note.is_empty() {
                out.push_str(&format!(", note: {}", quoted(&row.note)));
            }
            out.push_str("),\n");
        }
        out.push_str(&format!("{pad}],\n"));
        if let Some(catch_all) = &self.catch_all {
            out.push_str(&format!("{pad}catch_all: Some((result: "));
            write_result(out, &catch_all.result, depth + 1);
            let outputs: Vec<String> = catch_all
                .outputs
                .iter()
                .map(|(name, value)| format!("{}: {}", quoted(name), value_ron(value)))
                .collect();
            out.push_str(&format!(", outputs: {{{}}})),\n", outputs.join(", ")));
        }
        if let Some(near_best) = self.near_best {
            out.push_str(&format!("{pad}near_best: Some({near_best:?}),\n"));
        }
        if let Some(recency) = self.recency {
            out.push_str(&format!(
                "{pad}recency: Some((window: {}, penalty: {:?})),\n",
                recency.window, recency.penalty
            ));
        }
        out.push_str(&end);
        out.push(')');
    }
}

fn write_result(out: &mut String, result: &ResultEntry, depth: usize) {
    match result {
        ResultEntry::Asset(path) => out.push_str(&format!("Asset({})", quoted(path))),
        ResultEntry::Value(value) => out.push_str(&format!("Value({})", value_ron(value))),
        ResultEntry::Chooser(path) => out.push_str(&format!("Chooser({})", quoted(path))),
        ResultEntry::Nothing => out.push_str("Nothing"),
        ResultEntry::Inline(table) => {
            out.push_str("Inline(");
            table.write_body(out, depth, false);
            out.push(')');
        }
    }
}

/// RON text of a serde enum value through `ron` (unit and newtype variants only here).
fn ron_enum<T: serde::Serialize + fmt::Debug>(value: &T) -> String {
    ron::to_string(value).unwrap_or_else(|_| format!("{value:?}"))
}

fn kinds_ron(map: &BTreeMap<String, ValueKind>) -> String {
    let items: Vec<String> = map
        .iter()
        .map(|(name, kind)| format!("{}: {}", quoted(name), ron_enum(kind)))
        .collect();
    format!("{{{}}}", items.join(", "))
}

/// A string as RON text. Rust's `{:?}` is not RON: it writes a NUL as `\0`, which the RON parser refuses, so a saved
/// table would stop loading (ADR-1917).
fn quoted(text: &str) -> String {
    ron::to_string(text).unwrap_or_else(|_| format!("{text:?}"))
}

fn column_ron(column: &Column) -> String {
    let mut out = format!("(id: {}, ", quoted(&column.id));
    if let Some(input) = &column.input {
        out.push_str(&format!("input: {}, ", quoted(input)));
    }
    if let Some(output) = &column.output {
        out.push_str(&format!("output: {}, ", quoted(output)));
    }
    match &column.kind {
        ColumnKind::FloatDistance {
            influence,
            max_distance,
            wrap,
            drop_beyond,
        } => {
            out.push_str(&format!(
                "kind: FloatDistance(influence: {influence:?}, max_distance: {max_distance:?}"
            ));
            if let Some((lo, hi)) = wrap {
                out.push_str(&format!(", wrap: Some(({lo:?}, {hi:?}))"));
            }
            if *drop_beyond {
                out.push_str(", drop_beyond: true");
            }
            out.push(')');
        }
        kind => out.push_str(&format!("kind: {}", kind.ron_name())),
    }
    if let Some((lo, hi)) = column.wrap {
        out.push_str(&format!(", wrap: Some(({lo:?}, {hi:?}))"));
    }
    if !column.case_sensitive {
        out.push_str(", case_sensitive: false");
    }
    if column.disabled {
        out.push_str(", disabled: true");
    }
    out.push(')');
    out
}
