// SPDX-License-Identifier: MPL-2.0
//! Data tables: typed rows of one struct, named by a key, kept in RON or CSV
//! (`docs/specs/content-data.md` § 3).
//!
//! - [`DataTable<R>`] is the typed table a game reads (`table.get("rifle")`). The row struct `R`
//!   derives `Reflect` (its fields are the table's columns, with their types and doc comments)
//!   and serde's `Serialize`/`Deserialize` (cells are read and written as RON literals).
//! - `.table.ron` (format `data_table` v1) is the canonical file; `.table.csv` (RFC 4180, see
//!   [`crate::csv`]) is the spreadsheet form. Both parse through the same row builder, so a cell has
//!   the same meaning in either.
//! - [`RawTable`] is the untyped view the `regia table` CLI uses (keys, columns, cell texts).
//! - [`DataTables`] (a resource) holds every table the app registered with
//!   [`DataTableAppExt::register_data_table`], type-erased behind [`ErasedTable`] for agents, with
//!   hot reload in dev builds.

mod capture;
mod cooked;
mod raw;
mod runtime;

use std::collections::HashMap;
use std::sync::Arc;

use regia_core::version::VersionStep;
use regia_ecs::reflect::{TypeInfo, TypePath, Typed};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use cooked::{
    CookedDataTable, DATA_TABLE_COOKED_TYPE_PATH, DATA_TABLE_COOKED_VERSION,
    data_table_cooked_schema_fingerprint,
};
pub use raw::{CsvImport, RawRow, RawTable};
pub use runtime::{
    DataTableAppExt, DataTableRegistrations, DataTables, ErasedTable, PendingTableReloads, TableEditError,
    TableSlot, apply_data_table_reloads, collect_data_table_changes, load_registered_data_tables,
};

/// Current `.table.ron` format version.
pub const DATA_TABLE_FORMAT_VERSION: u32 = 1;

/// Migration steps of the `.table.ron` format, oldest first. v0 is a bare `[("key", (..)), ...]`
/// list with no envelope and no `row_type`; reading it already yields the v1 rows, so the step only
/// records the version (the row type stays unnamed, which a typed load accepts).
pub const DATA_TABLE_MIGRATIONS: &[VersionStep<RawTable>] = &[VersionStep {
    from: 0,
    to: 1,
    migrate: migrate_v0_to_v1,
}];

fn migrate_v0_to_v1(table: &mut RawTable) -> Result<(), String> {
    table.source_version = 1;
    Ok(())
}

/// The `from` versions of [`DATA_TABLE_MIGRATIONS`] (for the format registry).
#[must_use]
pub fn data_table_migration_sources() -> Vec<u32> {
    DATA_TABLE_MIGRATIONS.iter().map(|s| s.from).collect()
}

/// Reads any supported `.table.ron` version into the current shape.
///
/// # Errors
///
/// The file does not parse, is newer than [`DATA_TABLE_FORMAT_VERSION`], or a migration failed.
pub fn read_raw_ron(text: &str) -> Result<RawTable, TableIssue> {
    let mut table = RawTable::from_ron(text)?;
    let from = table.source_version;
    regia_core::version::migrate(&mut table, from, DATA_TABLE_MIGRATIONS, DATA_TABLE_FORMAT_VERSION)
        .map_err(|e| TableIssue::new(IssueCode::BadFile, e.to_string()))?;
    Ok(table)
}

/// The kind of a [`TableIssue`]; [`IssueCode::code`] is the stable text agents and the CLI print.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IssueCode {
    /// Two rows share a key.
    DuplicateKey,
    /// A row has an empty key.
    EmptyKey,
    /// A cell names a column the row struct does not have.
    UnknownColumn,
    /// A row (or the CSV header) lacks a column of the row struct.
    MissingColumn,
    /// A cell does not parse as its column's type.
    BadCell,
    /// The file's `row_type` is not the table's row struct.
    WrongRowType,
    /// A CSV record does not parse or has the wrong number of fields.
    BadRecord,
    /// A rule added with [`DataTable::with_rule`] rejected a row.
    Rule,
    /// The file is not a table (syntax error, missing envelope field).
    BadFile,
    /// The file is a newer format version than this engine reads.
    UnsupportedVersion,
}

impl IssueCode {
    /// The issue's stable code (`table_duplicate_key`, ...).
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::DuplicateKey => "table_duplicate_key",
            Self::EmptyKey => "table_empty_key",
            Self::UnknownColumn => "table_unknown_column",
            Self::MissingColumn => "table_missing_column",
            Self::BadCell => "table_bad_cell",
            Self::WrongRowType => "table_wrong_row_type",
            Self::BadRecord => "table_bad_record",
            Self::Rule => "table_rule",
            Self::BadFile => "table_bad_file",
            Self::UnsupportedVersion => "table_unsupported_version",
        }
    }
}

/// How serious a [`TableIssue`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Severity {
    /// The table is wrong; a reload with this issue is not applied.
    Error,
    /// Worth a look; the table still loads.
    Warning,
}

impl Severity {
    /// `"error"` or `"warning"`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// One problem found while reading or validating a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableIssue {
    /// What kind of problem.
    pub code: IssueCode,
    /// Error or warning.
    pub severity: Severity,
    /// Human-readable detail.
    pub message: String,
    /// Row key, when the issue belongs to a row.
    pub key: Option<String>,
    /// Column, when the issue belongs to a cell or column.
    pub column: Option<String>,
    /// Line in the source file (1-based), when known.
    pub line: Option<usize>,
}

impl TableIssue {
    /// An error with no location.
    #[must_use]
    pub fn new(code: IssueCode, message: String) -> Self {
        Self {
            code,
            severity: Severity::Error,
            message,
            key: None,
            column: None,
            line: None,
        }
    }

    /// Sets the row key.
    #[must_use]
    pub fn with_key(mut self, key: &str) -> Self {
        self.key = Some(key.to_owned());
        self
    }

    /// Sets the column.
    #[must_use]
    pub fn with_column(mut self, column: &str) -> Self {
        self.column = Some(column.to_owned());
        self
    }

    /// Sets the line.
    #[must_use]
    pub fn with_line(mut self, line: usize) -> Self {
        self.line = Some(line);
        self
    }

    /// Sets the severity.
    #[must_use]
    pub fn with_severity(mut self, severity: Severity) -> Self {
        self.severity = severity;
        self
    }

    /// Whether the issue is an error.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl std::fmt::Display for TableIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.severity.name(), self.code.code())?;
        if let Some(line) = self.line {
            write!(f, " line {line}")?;
        }
        if let Some(key) = &self.key {
            write!(f, " row `{key}`")?;
        }
        if let Some(column) = &self.column {
            write!(f, " column `{column}`")?;
        }
        write!(f, ": {}", self.message)
    }
}

/// A table that did not load: every error found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableLoadError {
    /// The issues (at least one error).
    pub issues: Vec<TableIssue>,
}

impl std::fmt::Display for TableLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let errors: Vec<String> = self.issues.iter().map(ToString::to_string).collect();
        f.write_str(&errors.join("; "))
    }
}

impl std::error::Error for TableLoadError {}

/// Which file form a table text is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableFormat {
    /// `.table.ron`.
    Ron,
    /// `.table.csv` (or any `.csv`).
    Csv,
}

impl TableFormat {
    /// The form of a path by its extension (`.csv` is CSV, anything else RON).
    #[must_use]
    pub fn of_path(path: &str) -> Self {
        if path.to_ascii_lowercase().ends_with(".csv") {
            Self::Csv
        } else {
            Self::Ron
        }
    }

    /// `"ron"` or `"csv"`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Ron => "ron",
            Self::Csv => "csv",
        }
    }
}

/// One column of a table: a field of the row struct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    /// Field name (the CSV header and the RON field name).
    pub name: String,
    /// The field's type path (`u32`, `alloc::string::String`, `game::WeaponKind`).
    pub type_path: String,
    /// A `String` field: its CSV cells are plain text rather than RON literals.
    pub text: bool,
    /// The field's doc comment, when the row struct has one.
    pub doc: Option<String>,
}

/// The columns of row struct `R` from its reflection info; empty when `R` is not a struct with
/// named fields.
#[must_use]
pub fn columns_of<R: Typed>() -> Vec<Column> {
    match R::type_info() {
        TypeInfo::Struct(info) => info
            .iter()
            .map(|field| Column {
                name: field.name().to_owned(),
                type_path: field.type_path().to_owned(),
                text: field.is::<String>(),
                doc: field.docs().map(|d| d.trim().to_owned()),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A row-level rule: `Err(message)` rejects the row.
pub type RowRule<R> = Arc<dyn Fn(&str, &R) -> Result<(), String> + Send + Sync>;

/// A typed data table: rows of `R` in file order, looked up by key.
pub struct DataTable<R> {
    row_type: String,
    rows: Vec<(String, R)>,
    lines: Vec<Option<usize>>,
    index: HashMap<String, usize>,
    rules: Vec<RowRule<R>>,
}

impl<R: Clone> Clone for DataTable<R> {
    fn clone(&self) -> Self {
        Self {
            row_type: self.row_type.clone(),
            rows: self.rows.clone(),
            lines: self.lines.clone(),
            index: self.index.clone(),
            rules: self.rules.clone(),
        }
    }
}

impl<R> std::fmt::Debug for DataTable<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataTable")
            .field("row_type", &self.row_type)
            .field("rows", &self.rows.len())
            .field("rules", &self.rules.len())
            .finish()
    }
}

impl<R: Typed + Serialize + DeserializeOwned + Send + Sync + 'static> Default for DataTable<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: Typed + Serialize + DeserializeOwned + Send + Sync + 'static> DataTable<R> {
    /// An empty table of `R`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            row_type: R::type_path().to_owned(),
            rows: Vec::new(),
            lines: Vec::new(),
            index: HashMap::new(),
            rules: Vec::new(),
        }
    }

    /// Adds a row rule (checked by [`Self::validate`] and before a reload is applied).
    #[must_use]
    pub fn with_rule(
        mut self,
        rule: impl Fn(&str, &R) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        self.rules.push(Arc::new(rule));
        self
    }

    /// Adds a row rule to a loaded table (see [`Self::with_rule`]).
    pub fn add_rule(&mut self, rule: impl Fn(&str, &R) -> Result<(), String> + Send + Sync + 'static) {
        self.rules.push(Arc::new(rule));
    }

    /// Reads a `.table.ron` text (any supported version).
    ///
    /// # Errors
    ///
    /// Every syntax, row-type and cell error found ([`TableLoadError`]). Duplicate keys and rule
    /// failures are not load errors; [`Self::validate`] reports them.
    pub fn from_ron_str(text: &str) -> Result<Self, TableLoadError> {
        let raw = read_raw_ron(text).map_err(|issue| TableLoadError { issues: vec![issue] })?;
        Self::from_raw(&raw, false)
    }

    /// Reads a CSV table (header `key,<field>,...`).
    ///
    /// # Errors
    ///
    /// Every record, header and cell error found ([`TableLoadError`]).
    pub fn from_csv_str(text: &str) -> Result<Self, TableLoadError> {
        let columns = columns_of::<R>();
        let import = CsvImport {
            text_columns: columns
                .iter()
                .filter(|c| c.text)
                .map(|c| c.name.clone())
                .collect(),
            literal_columns: columns
                .iter()
                .filter(|c| !c.text)
                .map(|c| c.name.clone())
                .collect(),
        };
        let raw =
            RawTable::from_csv(text, &import).map_err(|issue| TableLoadError { issues: vec![issue] })?;
        Self::from_raw(&raw, true)
    }

    /// Reads `text` in `format`.
    ///
    /// # Errors
    ///
    /// See [`Self::from_ron_str`] and [`Self::from_csv_str`].
    pub fn parse(text: &str, format: TableFormat) -> Result<Self, TableLoadError> {
        match format {
            TableFormat::Ron => Self::from_ron_str(text),
            TableFormat::Csv => Self::from_csv_str(text),
        }
    }

    /// Builds typed rows from an untyped table. `from_csv`: a non-text cell that does not parse is
    /// tried once more as a string (a `PathBuf` column holds `ui/rifle.png`, not `"ui/rifle.png"`).
    ///
    /// # Errors
    ///
    /// Every row-type, column and cell error found.
    pub fn from_raw(raw: &RawTable, from_csv: bool) -> Result<Self, TableLoadError> {
        let mut table = Self::new();
        let mut issues = Vec::new();
        if !row_type_matches::<R>(&raw.row_type) {
            issues.push(TableIssue::new(
                IssueCode::WrongRowType,
                format!(
                    "the file's rows are `{}`, this table holds `{}`",
                    raw.row_type,
                    R::type_path()
                ),
            ));
            return Err(TableLoadError { issues });
        }
        let columns = columns_of::<R>();
        if columns.is_empty() {
            issues.push(TableIssue::new(
                IssueCode::WrongRowType,
                format!("`{}` is not a struct with named fields", R::type_path()),
            ));
            return Err(TableLoadError { issues });
        }
        if from_csv {
            for column in &raw.columns {
                if !columns.iter().any(|c| &c.name == column) {
                    issues.push(
                        TableIssue::new(
                            IssueCode::UnknownColumn,
                            format!("`{}` has no field `{column}`", R::type_path()),
                        )
                        .with_column(column)
                        .with_line(1),
                    );
                }
            }
        }
        for row in &raw.rows {
            match build_row::<R>(&row.cells, &columns, from_csv) {
                Ok(value) => table.push(row.key.clone(), value, Some(row.line)),
                Err(mut found) => {
                    for issue in &mut found {
                        issue.key = Some(row.key.clone());
                        issue.line = Some(row.line);
                    }
                    issues.extend(found);
                }
            }
        }
        if issues.iter().any(TableIssue::is_error) {
            return Err(TableLoadError { issues });
        }
        Ok(table)
    }

    fn push(&mut self, key: String, row: R, line: Option<usize>) {
        self.index.entry(key.clone()).or_insert(self.rows.len());
        self.rows.push((key, row));
        self.lines.push(line);
    }

    fn reindex(&mut self) {
        self.index.clear();
        for (i, (key, _)) in self.rows.iter().enumerate() {
            self.index.entry(key.clone()).or_insert(i);
        }
    }

    /// The row type path files must name.
    #[must_use]
    pub fn row_type(&self) -> &str {
        &self.row_type
    }

    /// The columns (fields of `R`).
    #[must_use]
    pub fn columns(&self) -> Vec<Column> {
        columns_of::<R>()
    }

    /// The row named `key` (the first one, if a key is duplicated).
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&R> {
        self.index.get(key).map(|&i| &self.rows[i].1)
    }

    /// The row named `key`, mutably.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut R> {
        let i = *self.index.get(key)?;
        Some(&mut self.rows[i].1)
    }

    /// Whether a row is named `key`.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }

    /// Sets the row named `key` (in place when it exists, appended otherwise); returns the old row.
    pub fn insert(&mut self, key: &str, row: R) -> Option<R> {
        if let Some(&i) = self.index.get(key) {
            return Some(std::mem::replace(&mut self.rows[i].1, row));
        }
        self.push(key.to_owned(), row, None);
        None
    }

    /// Removes every row named `key`; returns the first.
    pub fn remove(&mut self, key: &str) -> Option<R> {
        let mut first = None;
        let mut i = 0;
        while i < self.rows.len() {
            if self.rows[i].0 == key {
                let (_, row) = self.rows.remove(i);
                self.lines.remove(i);
                first.get_or_insert(row);
            } else {
                i += 1;
            }
        }
        self.reindex();
        first
    }

    /// Row keys in file order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.rows.iter().map(|(k, _)| k.as_str())
    }

    /// `(key, row)` in file order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &R)> {
        self.rows.iter().map(|(k, r)| (k.as_str(), r))
    }

    /// Number of rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the table has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Duplicate and empty keys, and every rule failure.
    #[must_use]
    pub fn validate(&self) -> Vec<TableIssue> {
        let mut issues = Vec::new();
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for (i, (key, row)) in self.rows.iter().enumerate() {
            let line = self.lines.get(i).copied().flatten();
            let locate = |issue: TableIssue| match line {
                Some(line) => issue.with_line(line),
                None => issue,
            };
            if key.trim().is_empty() {
                issues.push(locate(TableIssue::new(
                    IssueCode::EmptyKey,
                    "a row has an empty key".to_owned(),
                )));
            } else if seen.insert(key, i).is_some() {
                issues.push(locate(
                    TableIssue::new(
                        IssueCode::DuplicateKey,
                        format!("key `{key}` is used by more than one row"),
                    )
                    .with_key(key),
                ));
            }
            for rule in &self.rules {
                if let Err(message) = rule(key, row) {
                    issues.push(locate(TableIssue::new(IssueCode::Rule, message).with_key(key)));
                }
            }
        }
        issues
    }

    /// The cells of one row as `(column, RON text)` in declaration order.
    ///
    /// # Errors
    ///
    /// The row does not serialize as a struct.
    pub fn row_cells(row: &R) -> Result<Vec<(String, String)>, String> {
        capture::row_fields(row)
            .map(|fields| fields.into_iter().map(|(n, t)| (n.to_owned(), t)).collect())
            .map_err(|e| e.to_string())
    }

    /// The untyped view of the table (cells as RON texts).
    ///
    /// # Errors
    ///
    /// A row does not serialize as a struct.
    pub fn to_raw(&self) -> Result<RawTable, String> {
        let mut raw = RawTable {
            row_type: self.row_type.clone(),
            source_version: DATA_TABLE_FORMAT_VERSION,
            columns: columns_of::<R>().into_iter().map(|c| c.name).collect(),
            rows: Vec::with_capacity(self.rows.len()),
        };
        for (i, (key, row)) in self.rows.iter().enumerate() {
            raw.rows.push(RawRow {
                key: key.clone(),
                line: self.lines.get(i).copied().flatten().unwrap_or(0),
                cells: Self::row_cells(row)?,
            });
        }
        Ok(raw)
    }

    /// The table as `.table.ron` text (v1).
    ///
    /// # Errors
    ///
    /// A row does not serialize as a struct.
    pub fn to_ron_string(&self) -> Result<String, String> {
        Ok(self.to_raw()?.to_ron())
    }

    /// The table as CSV text (header `key,<fields>`, CRLF records).
    ///
    /// # Errors
    ///
    /// A row does not serialize as a struct.
    pub fn to_csv_string(&self) -> Result<String, String> {
        Ok(self.to_raw()?.to_csv())
    }

    /// Copies the rules of `other` (a reload keeps the game's rules).
    pub(crate) fn take_rules_from(&mut self, other: &Self) {
        self.rules = other.rules.clone();
    }

    pub(crate) fn position(&self, key: &str) -> Option<usize> {
        self.index.get(key).copied()
    }

    pub(crate) fn replace_at(&mut self, i: usize, row: R) {
        self.rows[i].1 = row;
    }

    pub(crate) fn row_at(&self, i: usize) -> &R {
        &self.rows[i].1
    }
}

/// Whether a file's `row_type` names `R`: empty (unnamed, format v0), the full type path, the
/// short path, or the full path spelled with the engine's pre-rename crate names.
fn row_type_matches<R: TypePath>(written: &str) -> bool {
    let written = written.trim();
    written.is_empty()
        || written == R::type_path()
        || written == R::short_type_path()
        || regia_core::brand::current_type_path(written) == R::type_path()
}

/// Deserializes one row from its cells. Errors carry the column they belong to when known.
pub(crate) fn build_row<R: DeserializeOwned>(
    cells: &[(String, String)],
    columns: &[Column],
    from_csv: bool,
) -> Result<R, Vec<TableIssue>> {
    let mut issues = Vec::new();
    let mut cells: Vec<(String, String)> = cells
        .iter()
        .filter(|(name, _)| {
            let known = columns.iter().any(|c| &c.name == name);
            if !known && !from_csv {
                issues.push(
                    TableIssue::new(
                        IssueCode::UnknownColumn,
                        format!("the row struct has no field `{name}`"),
                    )
                    .with_column(name),
                );
            }
            known
        })
        .cloned()
        .collect();
    if !issues.is_empty() {
        return Err(issues);
    }
    let mut retried = vec![false; cells.len()];
    loop {
        let (text, starts) = row_text(&cells);
        match ron::from_str::<R>(&text) {
            Ok(row) => return Ok(row),
            Err(error) => {
                let column = match &error.code {
                    ron::Error::MissingStructField { field, .. } => {
                        return Err(vec![
                            TableIssue::new(
                                IssueCode::MissingColumn,
                                format!("no value for `{field}` and it has no default"),
                            )
                            .with_column(field),
                        ]);
                    }
                    _ => cell_at(&starts, error.span.end.line, error.span.end.col),
                };
                if let Some(i) = column
                    && from_csv
                    && !retried[i]
                    && !is_string_literal(&cells[i].1)
                {
                    retried[i] = true;
                    let quoted = raw::quote(&cells[i].1);
                    cells[i].1 = quoted;
                    continue;
                }
                let mut issue = TableIssue::new(IssueCode::BadCell, error.code.to_string());
                if let Some(i) = column {
                    issue = issue.with_column(&cells[i].0);
                    if from_csv && retried[i] {
                        // Report the cell as the author wrote it.
                        issue.message = format!("{} (cell `{}`)", issue.message, raw::csv_cell(&cells[i].1));
                    }
                }
                return Err(vec![issue]);
            }
        }
    }
}

/// `(name: value, ...)` and the (line, column) where each cell (its field name) starts (1-based, in chars, as
/// RON reports error positions).
fn row_text(cells: &[(String, String)]) -> (String, Vec<(usize, usize)>) {
    let mut text = String::from("(");
    let mut starts = Vec::with_capacity(cells.len());
    let (mut line, mut col) = (1usize, 2usize);
    fn push(text: &mut String, piece: &str, line: &mut usize, col: &mut usize) {
        for c in piece.chars() {
            if c == '\n' {
                *line += 1;
                *col = 1;
            } else {
                *col += 1;
            }
        }
        text.push_str(piece);
    }
    for (name, value) in cells {
        // A cell's span starts at its field name: RON reports some errors at the name it was
        // about to read, others after the value it failed on.
        starts.push((line, col));
        push(&mut text, &format!("{name}: "), &mut line, &mut col);
        push(&mut text, value, &mut line, &mut col);
        push(&mut text, ", ", &mut line, &mut col);
    }
    text.push(')');
    (text, starts)
}

/// The cell that starts at or before (`line`, `col`).
fn cell_at(starts: &[(usize, usize)], line: usize, col: usize) -> Option<usize> {
    starts.iter().rposition(|&start| start <= (line, col))
}

/// Whether a RON text is a string literal (`"..."` or a raw string).
pub(crate) fn is_string_literal(text: &str) -> bool {
    let text = text.trim();
    (text.starts_with('"') || text.starts_with("r\"") || text.starts_with("r#")) && unquote(text).is_some()
}

/// The text of a RON string literal.
pub(crate) fn unquote(text: &str) -> Option<String> {
    ron::from_str::<String>(text.trim()).ok()
}

/// How a cell's RON text reads in a grid or a CSV file: a string literal's text, any other literal as
/// written (`"ui/rifle.png"` reads `ui/rifle.png`, `Burst(3)` reads `Burst(3)`).
#[must_use]
pub fn cell_display_text(ron_text: &str) -> String {
    raw::csv_cell(ron_text)
}

/// The RON text a typed-in cell stores, by the CSV cell rules: in a plain-text column (`text`), anything
/// but a string literal becomes a string; elsewhere a text that is one RON value stays as written and any
/// other text becomes a string (a `PathBuf` cell typed as `sfx/a.wav`).
#[must_use]
pub fn cell_literal_from_text(text: &str, text_column: bool) -> String {
    let trimmed = text.trim();
    if is_string_literal(trimmed) || (!text_column && raw::is_one_value(trimmed)) {
        trimmed.to_owned()
    } else {
        raw::quote(text)
    }
}

/// Whether `text` is exactly one RON value (the structural cell check of [`RawTable::validate`]).
#[must_use]
pub fn is_one_ron_value(text: &str) -> bool {
    raw::is_one_value(text)
}

/// The CSV import a row struct's columns imply: its `String` fields are plain text, every other field
/// holds RON literals (what [`DataTable::from_csv_str`] uses).
#[must_use]
pub fn csv_import_for(columns: &[Column]) -> CsvImport {
    CsvImport {
        text_columns: columns
            .iter()
            .filter(|c| c.text)
            .map(|c| c.name.clone())
            .collect(),
        literal_columns: columns
            .iter()
            .filter(|c| !c.text)
            .map(|c| c.name.clone())
            .collect(),
    }
}

/// Every field a `.table.ron` file has, for the format registry (`docs/specs/12-assets.md` § 4a):
/// the envelope's fields in order; the rows' shape is the game's row struct.
#[must_use]
pub fn data_table_schema_fingerprint() -> String {
    "DataTable{format_version:u32,row_type:String,rows:Map<String,Row>}".to_owned()
}
