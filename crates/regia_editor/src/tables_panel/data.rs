// SPDX-License-Identifier: MPL-2.0
//! The Tables panel's **Data** tab (ADR-2132): typed data tables (`.table.ron`, `.table.csv`,
//! `docs/specs/content-data.md` § 3) as a grid, one row per key and one column per field.
//!
//! - The **schema** is the row struct. When the edit world's `DataTables` holds the table (a game plugin
//!   registered it), every edit is checked as a load would check it ([`ErasedTable::check_raw`]: field
//!   types, unknown and missing columns, the game's row rules). Otherwise, when the type registry knows the
//!   file's `row_type`, its fields give the columns and their types and a cell naming no field is an error.
//!   Without either, the checks are structural (keys, one RON value per cell).
//! - **Schema errors** badge the offending cell (or the key, for key errors) and list under the grid. An
//!   edit that would add an error is refused with that error's code and changes nothing; errors the file
//!   already had stay visible until fixed.
//! - **Undo** keeps the inverse of each edit (32 per open table), not a copy of the table, so a large table
//!   costs a few bytes per step.
//! - Edits never write the file; `editor.tables_panel_save` does (canonical `.table.ron`, or CSV).

use std::collections::BTreeMap;
use std::path::Path;

use regia_asset::data_table::{
    Column, CsvImport, DataTables, ErasedTable, RawRow, RawTable, TableFormat, TableIssue, cell_display_text,
    cell_literal_from_text, csv_import_for, read_raw_ron,
};
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, TypeInfo};
use serde_json::{Value, json};

use super::{UNDO_DEPTH, err};
use crate::data_panels::{PanelError, str_param};

/// Fill of a cell (or key) with a schema error.
pub const ERROR_FILL: egui::Color32 = egui::Color32::from_rgb(140, 52, 30);
/// Fill of a cell with a warning (a row without a column other rows have).
pub const WARNING_FILL: egui::Color32 = egui::Color32::from_rgb(110, 90, 30);

/// Where the open table's schema comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaSource {
    /// The edit world's `DataTables` holds the table: every check a load runs.
    Loaded,
    /// The type registry knows the row struct: its fields and their types.
    Reflected,
    /// Neither: keys and one RON value per cell only.
    Structural,
}

impl SchemaSource {
    /// `loaded`, `reflected` or `structural`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::Reflected => "reflected",
            Self::Structural => "structural",
        }
    }
}

/// One edit of a data table, and also its inverse (what the undo stack holds).
#[derive(Clone, Debug)]
pub(super) enum DataOp {
    /// Sets the cell of the row at `row` (`None` clears it: the field's default applies).
    Cell {
        row: usize,
        column: String,
        value: Option<String>,
    },
    /// Inserts `row` at `at`.
    Insert { at: usize, row: RawRow },
    /// Removes the row at `at`.
    Remove { at: usize },
    /// Renames the row at `at`.
    Rename { at: usize, key: String },
    /// Moves the row at `from` to index `to`.
    Move { from: usize, to: usize },
    /// Replaces the whole table (an import) with `raw` and its file columns; its inverse holds the old
    /// table.
    Replace {
        raw: Box<RawTable>,
        base_columns: Vec<String>,
    },
}

/// An open data table.
pub struct DataDoc {
    /// Path under `assets/`.
    pub path: String,
    /// The file form (RON or CSV); Save writes the same form.
    pub format: TableFormat,
    /// The rows, cells as RON literal texts.
    pub raw: RawTable,
    /// The row struct's columns, when known.
    pub schema: Option<Vec<Column>>,
    /// Where [`Self::schema`] came from.
    pub source: SchemaSource,
    /// The table's issues (structural, then typed), deduplicated.
    pub issues: Vec<TableIssue>,
    /// Bumped by every applied edit.
    pub generation: u64,
    /// Whether there are unsaved edits.
    pub dirty: bool,
    base_columns: Vec<String>,
    undo: Vec<DataOp>,
    pub(super) history_id: u64,
    cell_text: BTreeMap<(String, String), String>,
    new_key: String,
}

/// What makes two issues the same one: code, row, column and whether it is an error (a structural
/// warning and a typed error may share the first three).
fn issue_key(issue: &TableIssue) -> (&'static str, Option<String>, Option<String>, bool) {
    (
        issue.code.code(),
        issue.key.clone(),
        issue.column.clone(),
        issue.is_error(),
    )
}

/// The row struct named `row_type` in the type registry, as columns.
fn reflected_columns(registry: &AppTypeRegistry, row_type: &str) -> Option<Vec<Column>> {
    let row_type = row_type.trim();
    if row_type.is_empty() {
        return None;
    }
    let registry = registry.read();
    let registration = registry
        .get_with_type_path(row_type)
        .or_else(|| registry.get_with_short_type_path(row_type))
        .or_else(|| regia_core::brand::lookup_type_path(row_type, |p| registry.get_with_type_path(p)))?;
    match registration.type_info() {
        TypeInfo::Struct(info) => Some(
            info.iter()
                .map(|field| Column {
                    name: field.name().to_owned(),
                    type_path: field.type_path().to_owned(),
                    text: field.is::<String>(),
                    doc: field.docs().map(|d| d.trim().to_owned()),
                })
                .collect(),
        ),
        _ => None,
    }
}

/// What the world knows about a table's schema: the loaded table, else the reflected row struct.
pub(super) struct SchemaLookup<'w> {
    tables: Option<&'w DataTables>,
    registry: Option<&'w AppTypeRegistry>,
}

impl<'w> SchemaLookup<'w> {
    pub(super) fn of(world: &'w World) -> Self {
        Self {
            tables: world.get_resource::<DataTables>(),
            registry: world.get_resource::<AppTypeRegistry>(),
        }
    }

    pub(super) const fn none() -> Self {
        Self {
            tables: None,
            registry: None,
        }
    }

    fn loaded(&self, path: &str) -> Option<&'w dyn ErasedTable> {
        self.tables.and_then(|t| t.slot(path)).map(|slot| &*slot.table)
    }
}

impl DataDoc {
    /// Opens `text` (the file at `path`) with the schema `lookup` knows.
    pub(super) fn open(path: &str, text: &str, lookup: &SchemaLookup<'_>) -> Result<Self, PanelError> {
        let format = TableFormat::of_path(path);
        let loaded = lookup.loaded(path);
        let raw = match format {
            TableFormat::Ron => read_raw_ron(text),
            TableFormat::Csv => {
                let import = loaded.map_or_else(CsvImport::default, |t| csv_import_for(&t.columns()));
                RawTable::from_csv(text, &import)
            }
        }
        .map_err(|issue| PanelError::new(issue.code.code(), issue.to_string()))?;
        let mut doc = Self {
            path: path.to_owned(),
            format,
            base_columns: raw.columns.clone(),
            raw,
            schema: None,
            source: SchemaSource::Structural,
            issues: Vec::new(),
            generation: 0,
            dirty: false,
            undo: Vec::new(),
            history_id: 0,
            cell_text: BTreeMap::new(),
            new_key: String::new(),
        };
        doc.check(lookup);
        Ok(doc)
    }

    /// Re-runs every check `lookup` allows and refreshes the schema.
    pub(super) fn check(&mut self, lookup: &SchemaLookup<'_>) {
        let _span = tracing::trace_span!(target: "regia_editor", "tables_panel_data_check").entered();
        let mut issues = self.raw.validate();
        if let Some(table) = lookup.loaded(&self.path) {
            self.schema = Some(table.columns());
            self.source = SchemaSource::Loaded;
            issues.extend(table.check_raw(&self.raw, self.format == TableFormat::Csv));
        } else if let Some(columns) = lookup
            .registry
            .and_then(|r| reflected_columns(r, &self.raw.row_type))
        {
            for row in &self.raw.rows {
                for (column, _) in &row.cells {
                    if !columns.iter().any(|c| &c.name == column) {
                        issues.push(
                            TableIssue::new(
                                regia_asset::data_table::IssueCode::UnknownColumn,
                                format!("the row struct has no field `{column}`"),
                            )
                            .with_key(&row.key)
                            .with_column(column)
                            .with_line(row.line),
                        );
                    }
                }
            }
            self.schema = Some(columns);
            self.source = SchemaSource::Reflected;
        } else {
            self.schema = None;
            self.source = SchemaSource::Structural;
        }
        let mut seen = std::collections::BTreeSet::new();
        issues.retain(|issue| seen.insert(issue_key(issue)));
        self.issues = issues;
    }

    /// The grid's columns: the schema's fields in declaration order, then any other column the rows
    /// use (an unknown column, badged).
    #[must_use]
    pub fn columns(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .schema
            .as_ref()
            .map(|s| s.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();
        for column in &self.raw.columns {
            if !out.contains(column) {
                out.push(column.clone());
            }
        }
        out
    }

    /// Whether `column` holds plain text (a `String` field; without a schema, a column whose every cell
    /// is a string).
    fn text_column(&self, column: &str) -> bool {
        match &self.schema {
            Some(schema) => schema.iter().any(|c| c.name == column && c.text),
            None => self.raw.string_columns().iter().any(|c| c == column),
        }
    }

    fn position(&self, key: &str) -> Result<usize, PanelError> {
        self.raw
            .rows
            .iter()
            .position(|r| r.key == key)
            .ok_or_else(|| err("row_not_found", format!("no row `{key}`")))
    }

    /// The worst issue at a cell (`column` `None`: the key cell).
    fn issue_at(&self, key: &str, column: Option<&str>) -> Option<&TableIssue> {
        let mut found: Option<&TableIssue> = None;
        for issue in &self.issues {
            let here = issue.key.as_deref() == Some(key) && issue.column.as_deref() == column;
            let key_cell = column.is_none() && issue.key.as_deref() == Some(key) && issue.column.is_none();
            if (here || key_cell) && found.is_none_or(|f| !f.is_error() && issue.is_error()) {
                found = Some(issue);
            }
        }
        found
    }

    /// `error`, `warning` or `ok` for a cell (`column` `None`: the key).
    #[must_use]
    pub fn cell_state(&self, key: &str, column: Option<&str>) -> &'static str {
        match self.issue_at(key, column) {
            Some(issue) if issue.is_error() => "error",
            Some(_) => "warning",
            None => "ok",
        }
    }

    /// Applies `op` to the rows; returns its inverse.
    fn apply(&mut self, op: DataOp) -> Result<DataOp, PanelError> {
        let rows = self.raw.rows.len();
        let out_of_range = |at: usize| err("row_not_found", format!("no row at {at} (the table has {rows})"));
        Ok(match op {
            DataOp::Cell { row, column, value } => {
                let target = self.raw.rows.get_mut(row).ok_or_else(|| out_of_range(row))?;
                let at = target.cells.iter().position(|(c, _)| *c == column);
                let old = at.map(|i| target.cells[i].1.clone());
                match (at, value) {
                    (Some(i), Some(v)) => target.cells[i].1 = v,
                    (Some(i), None) => {
                        target.cells.remove(i);
                    }
                    (None, Some(v)) => target.cells.push((column.clone(), v)),
                    (None, None) => {}
                }
                DataOp::Cell {
                    row,
                    column,
                    value: old,
                }
            }
            DataOp::Insert { at, row } => {
                if at > rows {
                    return Err(out_of_range(at));
                }
                self.raw.rows.insert(at, row);
                DataOp::Remove { at }
            }
            DataOp::Remove { at } => {
                if at >= rows {
                    return Err(out_of_range(at));
                }
                let row = self.raw.rows.remove(at);
                DataOp::Insert { at, row }
            }
            DataOp::Rename { at, key } => {
                let target = self.raw.rows.get_mut(at).ok_or_else(|| out_of_range(at))?;
                let old = std::mem::replace(&mut target.key, key);
                DataOp::Rename { at, key: old }
            }
            DataOp::Move { from, to } => {
                if from >= rows || to >= rows {
                    return Err(out_of_range(from.max(to)));
                }
                let row = self.raw.rows.remove(from);
                self.raw.rows.insert(to, row);
                DataOp::Move { from: to, to: from }
            }
            DataOp::Replace { raw, base_columns } => {
                let old_raw = std::mem::replace(&mut self.raw, *raw);
                let old_columns = std::mem::replace(&mut self.base_columns, base_columns);
                DataOp::Replace {
                    raw: Box::new(old_raw),
                    base_columns: old_columns,
                }
            }
        })
    }

    /// The file's columns, then every other column a row uses, in first-use order.
    fn refresh_columns(&mut self) {
        let mut columns = self.base_columns.clone();
        for row in &self.raw.rows {
            for (column, _) in &row.cells {
                if !columns.contains(column) {
                    columns.push(column.clone());
                }
            }
        }
        self.raw.columns = columns;
    }

    /// Applies `op` when it adds no error; refuses it (and changes nothing) otherwise.
    fn edit(&mut self, op: DataOp, lookup: &SchemaLookup<'_>) -> Result<(), PanelError> {
        let before: std::collections::BTreeSet<_> = self
            .issues
            .iter()
            .filter(|i| i.is_error())
            .map(issue_key)
            .collect();
        let inverse = self.apply(op)?;
        self.refresh_columns();
        self.check(lookup);
        if let Some(new) = self
            .issues
            .iter()
            .find(|i| i.is_error() && !before.contains(&issue_key(i)))
            .cloned()
        {
            // Put the rows back exactly as they were.
            let _ = self.apply(inverse);
            self.refresh_columns();
            self.check(lookup);
            return Err(PanelError::new(new.code.code(), new.to_string()));
        }
        self.undo.push(inverse);
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.generation += 1;
        self.dirty = true;
        self.cell_text.clear();
        Ok(())
    }

    /// Undoes the last edit.
    pub(super) fn undo(&mut self, lookup: &SchemaLookup<'_>) -> Result<(), PanelError> {
        let op = self
            .undo
            .pop()
            .ok_or_else(|| err("nothing_to_undo", "nothing to undo"))?;
        self.apply(op)?;
        self.refresh_columns();
        self.check(lookup);
        self.generation += 1;
        self.dirty = true;
        self.cell_text.clear();
        Ok(())
    }

    /// Number of undo steps held.
    #[must_use]
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    /// The table in `format` (the text an Export writes): `regia table to-csv|to-json` or the canonical
    /// `.table.ron`.
    ///
    /// # Errors
    ///
    /// A `table_*` code when the JSON form cannot be built (it cannot for a table read from a file).
    pub fn export_text(&self, format: super::exchange::ExchangeFormat) -> Result<String, PanelError> {
        use super::exchange::ExchangeFormat;
        Ok(match format {
            ExchangeFormat::Csv => self.raw.to_csv(),
            ExchangeFormat::Json => regia_asset_pipeline::table_json::to_json(&self.raw)
                .map_err(|issue| PanelError::new(issue.code.code(), issue.to_string()))?,
            ExchangeFormat::Ron => self.raw.to_ron(),
        })
    }

    /// Replaces the rows with the table `text` holds in `format`, as one undo step. The cell rules are
    /// those of `regia table to-ron --into` on this table: its row type is kept and a column that holds
    /// strings (or is a `String` field of the schema) stays text; any other column is read as RON
    /// literals. An import that would add a schema error is refused and changes nothing.
    ///
    /// # Errors
    ///
    /// The reader's `table_*` code (`table_bad_file`, `table_bad_record`) for text that does not parse,
    /// or the first new schema error's code.
    pub(super) fn import_text(
        &mut self,
        text: &str,
        format: super::exchange::ExchangeFormat,
        lookup: &SchemaLookup<'_>,
    ) -> Result<usize, PanelError> {
        use super::exchange::ExchangeFormat;
        let mut import = CsvImport::default();
        let strings = self.raw.string_columns();
        for column in self.columns() {
            if strings.contains(&column) || self.text_column(&column) {
                import.text_columns.push(column);
            } else {
                import.literal_columns.push(column);
            }
        }
        let parsed = match format {
            ExchangeFormat::Csv => RawTable::from_csv(text, &import),
            ExchangeFormat::Json => regia_asset_pipeline::table_json::from_json(text, &import),
            ExchangeFormat::Ron => read_raw_ron(text),
        }
        .map_err(|issue| PanelError::new(issue.code.code(), issue.to_string()))?;
        let mut raw = parsed;
        if !self.raw.row_type.is_empty() {
            raw.row_type.clone_from(&self.raw.row_type);
        }
        let rows = raw.rows.len();
        let base_columns = raw.columns.clone();
        self.edit(
            DataOp::Replace {
                raw: Box::new(raw),
                base_columns,
            },
            lookup,
        )?;
        Ok(rows)
    }

    /// The file text Save writes.
    #[must_use]
    pub fn text(&self) -> String {
        match self.format {
            TableFormat::Ron => self.raw.to_ron(),
            TableFormat::Csv => self.raw.to_csv(),
        }
    }

    /// Sets the cell at (`key`, `column`) from typed text (empty clears it).
    fn set_cell_text(
        &mut self,
        key: &str,
        column: &str,
        text: &str,
        lookup: &SchemaLookup<'_>,
    ) -> Result<(), PanelError> {
        let row = self.position(key)?;
        let value = if text.trim().is_empty() && !self.text_column(column) {
            None
        } else {
            Some(cell_literal_from_text(text, self.text_column(column)))
        };
        self.edit(
            DataOp::Cell {
                row,
                column: column.to_owned(),
                value,
            },
            lookup,
        )
    }

    /// The JSON form the agent reads.
    pub(super) fn json(&self) -> Value {
        let columns = self.columns();
        let schema = self.schema.as_ref();
        json!({
            "path": self.path,
            "format": self.format.name(),
            "row_type": self.raw.row_type,
            "schema": self.source.name(),
            "generation": self.generation,
            "dirty": self.dirty,
            "undo_depth": self.undo.len(),
            "columns": columns.iter().map(|name| {
                let field = schema.and_then(|s| s.iter().find(|c| &c.name == name));
                json!({
                    "name": name,
                    "type_path": field.map(|c| c.type_path.clone()),
                    "text": self.text_column(name),
                    "doc": field.and_then(|c| c.doc.clone()),
                    "known": schema.is_none() || field.is_some(),
                })
            }).collect::<Vec<_>>(),
            "rows": self.raw.rows.iter().map(|row| json!({
                "key": row.key,
                "line": row.line,
                "state": self.cell_state(&row.key, None),
                "cells": columns.iter().map(|column| {
                    let cell = row.cells.iter().find(|(c, _)| c == column).map(|(_, v)| v.clone());
                    json!({
                        "column": column,
                        "ron": cell,
                        "text": cell.as_deref().map(cell_display_text),
                        "state": self.cell_state(&row.key, Some(column)),
                        "issue": self.issue_at(&row.key, Some(column)).map(|i| i.code.code()),
                    })
                }).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "issues": self.issues.iter().map(|i| json!({
                "code": i.code.code(),
                "severity": i.severity.name(),
                "message": i.message,
                "key": i.key,
                "column": i.column,
                "line": i.line,
            })).collect::<Vec<_>>(),
            "errors": self.issues.iter().filter(|i| i.is_error()).count(),
            "warnings": self.issues.iter().filter(|i| !i.is_error()).count(),
        })
    }
}

/// Applies one Data-tab edit (`params.op`: `set_cell`, `clear_cell`, `insert_row`, `remove_row`,
/// `rename_row`, `move_row`).
pub(super) fn data_edit(
    doc: &mut DataDoc,
    params: &Value,
    lookup: &SchemaLookup<'_>,
) -> Result<(), PanelError> {
    let op = params.get("op").and_then(Value::as_str).unwrap_or_default();
    let field = |key: &str| -> Result<String, PanelError> {
        str_param(params, key)
            .map(str::to_owned)
            .map_err(|e| err("bad_value", e.message))
    };
    let before = |doc: &DataDoc| -> Result<usize, PanelError> {
        match params.get("before").and_then(Value::as_str) {
            None => Ok(doc.raw.rows.len()),
            Some(key) => doc.position(key),
        }
    };
    match op {
        "set_cell" => {
            let (key, column) = (field("key")?, field("column")?);
            if let Some(ron) = params.get("ron").and_then(Value::as_str) {
                let row = doc.position(&key)?;
                return doc.edit(
                    DataOp::Cell {
                        row,
                        column,
                        value: Some(ron.trim().to_owned()),
                    },
                    lookup,
                );
            }
            let text = match params.get("text") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                Some(Value::Bool(b)) => b.to_string(),
                _ => return Err(err("bad_value", "`text` (or `ron`) is required")),
            };
            doc.set_cell_text(&key, &column, &text, lookup)
        }
        "clear_cell" => {
            let (key, column) = (field("key")?, field("column")?);
            let row = doc.position(&key)?;
            doc.edit(
                DataOp::Cell {
                    row,
                    column,
                    value: None,
                },
                lookup,
            )
        }
        "insert_row" => {
            let key = field("key")?;
            let at = before(doc)?;
            let mut cells = Vec::new();
            if let Some(map) = params.get("cells").and_then(Value::as_object) {
                for (column, value) in map {
                    let text = match value {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    cells.push((
                        column.clone(),
                        cell_literal_from_text(&text, doc.text_column(column)),
                    ));
                }
            }
            let row = RawRow { key, line: 0, cells };
            doc.edit(DataOp::Insert { at, row }, lookup)
        }
        "remove_row" => {
            let at = doc.position(&field("key")?)?;
            doc.edit(DataOp::Remove { at }, lookup)
        }
        "rename_row" => {
            let at = doc.position(&field("key")?)?;
            let to = field("to")?;
            doc.edit(DataOp::Rename { at, key: to }, lookup)
        }
        "move_row" => {
            let from = doc.position(&field("key")?)?;
            let mut to = before(doc)?;
            if to > from {
                to -= 1;
            }
            let to = to.min(doc.raw.rows.len().saturating_sub(1));
            if to == from {
                return Ok(());
            }
            doc.edit(DataOp::Move { from, to }, lookup)
        }
        other => Err(err("bad_op", format!("unknown data edit `{other}`"))),
    }
}

/// Reads a data table under `assets/`.
pub(super) fn read_data(root: &Path, path: &str) -> Result<String, PanelError> {
    super::read(root, path, DATA_SUFFIXES)
}

/// The file suffixes the Data tab opens.
pub(super) const DATA_SUFFIXES: &[&str] = &[".table.ron", ".table.csv"];

// ------------------------------------------------------------------------------------------- drawing

fn fill(state: &str) -> egui::Color32 {
    match state {
        "error" => ERROR_FILL,
        "warning" => WARNING_FILL,
        _ => egui::Color32::TRANSPARENT,
    }
}

/// Draws the Data tab.
pub(super) fn draw_data(
    ui: &mut egui::Ui,
    doc: &mut DataDoc,
    lookup: &SchemaLookup<'_>,
) -> Result<(), PanelError> {
    let _span = tracing::trace_span!(target: "regia_editor", "tables_panel_data_draw").entered();
    ui.horizontal(|ui| {
        ui.label(format!(
            "{} · {} rows · schema: {}",
            if doc.raw.row_type.is_empty() {
                "(no row type)"
            } else {
                doc.raw.row_type.as_str()
            },
            doc.raw.rows.len(),
            doc.source.name()
        ));
    });
    let mut edits: Vec<Value> = Vec::new();
    let columns = doc.columns();
    // Only the rows in view are laid out (ADR-7306): a data table can hold thousands of rows. Row 0
    // of the scroll range is the header.
    let row_count = doc.raw.rows.len();
    let row_height = ui.spacing().interact_size.y + 4.0;
    egui::ScrollArea::both().id_salt("tables-data-grid").show_rows(ui, row_height, row_count + 1, |ui, range| {
        egui::Grid::new("tables-data").striped(true).show(ui, |ui| {
            if range.start == 0 {
            ui.strong("key");
            for column in &columns {
                let field = doc.schema.as_ref().and_then(|s| s.iter().find(|c| &c.name == column));
                ui.vertical(|ui| {
                    ui.strong(column);
                    let ty = field.map_or("?", |c| c.type_path.as_str());
                    let response = ui.weak(ty);
                    if let Some(doc_text) = field.and_then(|c| c.doc.as_deref()) {
                        response.on_hover_text(doc_text);
                    }
                });
            }
            ui.label("");
            ui.end_row();
            }
            let key_at = |doc: &DataDoc, index: usize| doc.raw.rows.get(index).map(|r| r.key.clone());
            for index in range.start.saturating_sub(1)..range.end.saturating_sub(1).min(row_count) {
                let Some(key) = key_at(doc, index) else { continue };
                let key = &key;
                let key_state = doc.cell_state(key, None);
                ui.horizontal(|ui| {
                    egui::Frame::new().fill(fill(key_state)).show(ui, |ui| {
                        let hover = doc.issue_at(key, None).map(ToString::to_string).unwrap_or_default();
                        ui.monospace(key).on_hover_text(hover);
                    });
                    if ui.add_enabled(index > 0, egui::Button::new("▲").small()).clicked() {
                        let before = index.checked_sub(1).and_then(|i| key_at(doc, i));
                        edits.push(json!({ "op": "move_row", "key": key, "before": before }));
                    }
                    if ui.add_enabled(index + 1 < row_count, egui::Button::new("▼").small()).clicked() {
                        edits.push(json!({ "op": "move_row", "key": key, "before": key_at(doc, index + 2) }));
                    }
                });
                for column in &columns {
                    let state = doc.cell_state(key, Some(column));
                    let issue = doc.issue_at(key, Some(column)).map(ToString::to_string);
                    let current = doc.raw.rows[index]
                        .cells
                        .iter()
                        .find(|(c, _)| c == column)
                        .map(|(_, v)| cell_display_text(v))
                        .unwrap_or_default();
                    let text = doc
                        .cell_text
                        .entry((key.clone(), column.clone()))
                        .or_insert_with(|| current.clone());
                    egui::Frame::new().fill(fill(state)).inner_margin(2.0).show(ui, |ui| {
                        let mut response = ui.add(egui::TextEdit::singleline(text).desired_width(90.0));
                        if let Some(issue) = &issue {
                            response = response.on_hover_text(issue);
                        }
                        if response.lost_focus() && *text != current {
                            edits.push(json!({ "op": "set_cell", "key": key, "column": column, "text": text.clone() }));
                        }
                    });
                }
                if ui.small_button("\u{1f5d9}").on_hover_text("Remove the row").clicked() {
                    edits.push(json!({ "op": "remove_row", "key": key }));
                }
                ui.end_row();
            }
        });
    });
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut doc.new_key)
                .hint_text("new key")
                .desired_width(120.0),
        );
        if ui.button("Add row").clicked() && !doc.new_key.trim().is_empty() {
            edits.push(json!({ "op": "insert_row", "key": doc.new_key.trim() }));
            doc.new_key.clear();
        }
    });
    for issue in &doc.issues {
        let color = if issue.is_error() {
            egui::Color32::from_rgb(235, 110, 90)
        } else {
            egui::Color32::from_rgb(230, 170, 80)
        };
        ui.colored_label(color, issue.to_string());
    }
    for edit in edits {
        if let Err(e) = data_edit(doc, &edit, lookup) {
            // A refused cell keeps the table; the box shows the stored text again.
            doc.cell_text.clear();
            return Err(e);
        }
    }
    Ok(())
}

impl DataDoc {
    /// Transfers standalone inverse operations to the hosted shared stack.
    pub(super) fn take_history(&mut self) -> Vec<DataOp> {
        std::mem::take(&mut self.undo)
    }
    /// Applies an inverse and replaces it with its redo, refreshing validation and dirty state.
    pub(super) fn replay_history(
        &mut self,
        op: &mut DataOp,
        lookup: &SchemaLookup<'_>,
    ) -> Result<(), PanelError> {
        let inverse = self.apply(op.clone())?;
        *op = inverse;
        self.refresh_columns();
        self.check(lookup);
        self.generation += 1;
        self.dirty = true;
        self.cell_text.clear();
        Ok(())
    }
}

impl DataOp {
    /// The edited row or cell path for shared history details.
    pub(super) fn property(&self) -> String {
        match self {
            Self::Cell { row, column, .. } => format!("rows.{row}.{column}"),
            Self::Insert { at, .. } | Self::Remove { at } => format!("rows.{at}"),
            Self::Rename { at, .. } => format!("rows.{at}.key"),
            Self::Move { from, .. } => format!("rows.{from}"),
            // An import replaces every row.
            Self::Replace { .. } => "rows".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests;
