// SPDX-License-Identifier: MPL-2.0
//! Loaded tables in the app: [`DataTables`], type-erased access for agents, registration and hot
//! reload (`docs/specs/content-data.md` § 3.5).

use std::any::Any;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_ecs::reflect::Typed;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use super::{
    Column, DataTable, IssueCode, TableFormat, TableIssue, TableLoadError, build_row, columns_of,
    is_string_literal, raw,
};
use crate::SourceAssetChanged;

/// Why an edit through [`ErasedTable`] was refused. [`TableEditError::code`] is the stable text.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum TableEditError {
    /// No row has the key.
    #[error("no row named `{0}`")]
    UnknownKey(String),
    /// The row struct has no such field.
    #[error("the row struct has no field `{0}`")]
    UnknownColumn(String),
    /// A row with the key already exists.
    #[error("a row named `{0}` already exists")]
    DuplicateKey(String),
    /// The key is empty.
    #[error("a row key must not be empty")]
    EmptyKey,
    /// The value does not parse as the column's type (or the row lacks a field with no default).
    #[error("bad value for `{column}`: {message}")]
    BadCell {
        /// Column.
        column: String,
        /// Parse error.
        message: String,
    },
    /// A row rule rejected the edited row.
    #[error("rule failed: {0}")]
    Rule(String),
    /// The row could not be written back as cells.
    #[error("{0}")]
    Encode(String),
}

impl TableEditError {
    /// Stable code: `table_unknown_key`, `table_unknown_column`, `table_duplicate_key`,
    /// `table_empty_key`, `table_bad_cell`, `table_rule`, `table_encode`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownKey(_) => "table_unknown_key",
            Self::UnknownColumn(_) => "table_unknown_column",
            Self::DuplicateKey(_) => "table_duplicate_key",
            Self::EmptyKey => "table_empty_key",
            Self::BadCell { .. } => "table_bad_cell",
            Self::Rule(_) => "table_rule",
            Self::Encode(_) => "table_encode",
        }
    }
}

/// A table seen without its row type: what agents and tools use.
///
/// Cell values are RON literals (`30`, `Rifle`, `(x: 1)`); for a `String` column plain text is
/// accepted too and becomes the string, as in a CSV cell.
pub trait ErasedTable: Send + Sync + 'static {
    /// The row struct's type path.
    fn row_type(&self) -> &str;
    /// The columns (fields of the row struct).
    fn columns(&self) -> Vec<Column>;
    /// Row keys in order.
    fn keys(&self) -> Vec<String>;
    /// Number of rows.
    fn len(&self) -> usize;
    /// Whether the table has no rows.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// The row named `key` as `(column, RON text)`.
    ///
    /// # Errors
    ///
    /// [`TableEditError::UnknownKey`], or the row does not serialize.
    fn row(&self, key: &str) -> Result<Vec<(String, String)>, TableEditError>;
    /// Sets one cell. The edit applies only when the row still parses and passes the rules.
    ///
    /// # Errors
    ///
    /// The key or column is unknown, the value does not parse, or a rule fails.
    fn set_cell(&mut self, key: &str, column: &str, value: &str) -> Result<(), TableEditError>;
    /// Adds a row from cells (fields left out take their serde default).
    ///
    /// # Errors
    ///
    /// The key is empty or taken, a column is unknown, a value does not parse, or a rule fails.
    fn insert_row(&mut self, key: &str, cells: &[(String, String)]) -> Result<(), TableEditError>;
    /// Removes the rows named `key`; whether any existed.
    fn remove_row(&mut self, key: &str) -> bool;
    /// Duplicate and empty keys and rule failures.
    fn validate(&self) -> Vec<TableIssue>;
    /// The table as text in `format`.
    ///
    /// # Errors
    ///
    /// A row does not serialize as a struct.
    fn export(&self, format: TableFormat) -> Result<String, String>;
    /// Replaces the rows with `text` when it parses and validates without errors (warnings are
    /// returned); otherwise keeps the current rows and returns the issues.
    ///
    /// # Errors
    ///
    /// Every issue of a text that was not applied.
    fn reload(&mut self, text: &str, format: TableFormat) -> Result<Vec<TableIssue>, Vec<TableIssue>>;
    /// Checks an untyped table against this table's row struct and rules without changing anything:
    /// the issues a load of `raw` would report (row type, unknown and missing columns, cells that do
    /// not parse as their field's type, duplicate and empty keys, rule failures). `from_csv` applies
    /// the CSV cell rules. The editor's Tables panel uses it for schema errors (ADR-2132). The default
    /// checks nothing.
    fn check_raw(&self, raw: &raw::RawTable, from_csv: bool) -> Vec<TableIssue> {
        let _ = (raw, from_csv);
        Vec::new()
    }
    /// The typed table, for downcasting.
    fn as_any(&self) -> &dyn Any;
    /// The typed table, mutably.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<R: Typed + Serialize + DeserializeOwned + Send + Sync + 'static> DataTable<R> {
    /// Checks the rules for one row.
    fn check_rules(&self, key: &str, row: &R) -> Result<(), TableEditError> {
        for rule in &self.rules {
            rule(key, row).map_err(TableEditError::Rule)?;
        }
        Ok(())
    }

    /// Builds a row from `(column, value)` cells (CSV cell rules) and maps the issues to an edit error.
    fn row_from_cells(cells: &[(String, String)], columns: &[Column]) -> Result<R, TableEditError> {
        let cells: Vec<(String, String)> = cells
            .iter()
            .map(|(name, value)| {
                let text = columns.iter().any(|c| &c.name == name && c.text) && !is_string_literal(value);
                (name.clone(), if text { raw::quote(value) } else { value.clone() })
            })
            .collect();
        build_row::<R>(&cells, columns, true).map_err(|issues| {
            let issue = issues.into_iter().next();
            match issue {
                Some(issue) if issue.code == IssueCode::UnknownColumn => {
                    TableEditError::UnknownColumn(issue.column.unwrap_or_default())
                }
                Some(issue) => TableEditError::BadCell {
                    column: issue.column.unwrap_or_default(),
                    message: issue.message,
                },
                None => TableEditError::Encode("the row did not parse".to_owned()),
            }
        })
    }
}

impl<R: Typed + Serialize + DeserializeOwned + Send + Sync + 'static> ErasedTable for DataTable<R> {
    fn row_type(&self) -> &str {
        DataTable::row_type(self)
    }

    fn columns(&self) -> Vec<Column> {
        columns_of::<R>()
    }

    fn keys(&self) -> Vec<String> {
        DataTable::keys(self).map(str::to_owned).collect()
    }

    fn len(&self) -> usize {
        DataTable::len(self)
    }

    fn row(&self, key: &str) -> Result<Vec<(String, String)>, TableEditError> {
        let row = self
            .get(key)
            .ok_or_else(|| TableEditError::UnknownKey(key.to_owned()))?;
        Self::row_cells(row).map_err(TableEditError::Encode)
    }

    fn set_cell(&mut self, key: &str, column: &str, value: &str) -> Result<(), TableEditError> {
        let columns = columns_of::<R>();
        if !columns.iter().any(|c| c.name == column) {
            return Err(TableEditError::UnknownColumn(column.to_owned()));
        }
        let i = self
            .position(key)
            .ok_or_else(|| TableEditError::UnknownKey(key.to_owned()))?;
        let mut cells = Self::row_cells(self.row_at(i)).map_err(TableEditError::Encode)?;
        match cells.iter_mut().find(|(c, _)| c == column) {
            Some(cell) => cell.1 = value.to_owned(),
            None => cells.push((column.to_owned(), value.to_owned())),
        }
        // The row's other cells are RON literals already; only the edited value may be plain text.
        let row = Self::row_from_cells(&cells, &columns)?;
        self.check_rules(key, &row)?;
        self.replace_at(i, row);
        Ok(())
    }

    fn insert_row(&mut self, key: &str, cells: &[(String, String)]) -> Result<(), TableEditError> {
        if key.trim().is_empty() {
            return Err(TableEditError::EmptyKey);
        }
        if self.contains_key(key) {
            return Err(TableEditError::DuplicateKey(key.to_owned()));
        }
        let columns = columns_of::<R>();
        if let Some((unknown, _)) = cells
            .iter()
            .find(|(c, _)| !columns.iter().any(|col| &col.name == c))
        {
            return Err(TableEditError::UnknownColumn(unknown.clone()));
        }
        let row = Self::row_from_cells(cells, &columns)?;
        self.check_rules(key, &row)?;
        self.insert(key, row);
        Ok(())
    }

    fn remove_row(&mut self, key: &str) -> bool {
        self.remove(key).is_some()
    }

    fn validate(&self) -> Vec<TableIssue> {
        DataTable::validate(self)
    }

    fn export(&self, format: TableFormat) -> Result<String, String> {
        match format {
            TableFormat::Ron => self.to_ron_string(),
            TableFormat::Csv => self.to_csv_string(),
        }
    }

    fn reload(&mut self, text: &str, format: TableFormat) -> Result<Vec<TableIssue>, Vec<TableIssue>> {
        let mut fresh = DataTable::<R>::parse(text, format).map_err(|e: TableLoadError| e.issues)?;
        fresh.take_rules_from(self);
        let issues = fresh.validate();
        if issues.iter().any(TableIssue::is_error) {
            return Err(issues);
        }
        *self = fresh;
        Ok(issues)
    }

    fn check_raw(&self, raw: &raw::RawTable, from_csv: bool) -> Vec<TableIssue> {
        // `from_raw` stops at the first failing table; this keeps going so every row is checked: each row
        // that builds also meets the rules, and the keys are checked as `validate` checks them.
        if let Err(error) = DataTable::<R>::from_raw(&raw.clone_shape(), from_csv) {
            return error.issues;
        }
        let columns = columns_of::<R>();
        let mut fresh = DataTable::<R>::new();
        fresh.take_rules_from(self);
        let mut issues = Vec::new();
        for row in &raw.rows {
            match build_row::<R>(&row.cells, &columns, from_csv) {
                Ok(value) => fresh.push(row.key.clone(), value, Some(row.line)),
                Err(found) => issues.extend(found.into_iter().map(|mut issue| {
                    issue.key = Some(row.key.clone());
                    issue.line = Some(row.line);
                    issue
                })),
            }
        }
        issues.extend(fresh.validate());
        issues
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// One loaded table and its bookkeeping.
pub struct TableSlot {
    /// The table.
    pub table: Box<dyn ErasedTable>,
    /// The file form it was read from.
    pub format: TableFormat,
    /// The file on disk, when known (dev builds with an asset server): where edits are saved.
    pub file: Option<PathBuf>,
    /// Bumped by every reload and every applied edit.
    pub generation: u64,
    /// Reloads applied.
    pub reloads: u64,
    /// Reloads refused because the new file had errors (the old rows stayed).
    pub rejected_reloads: u64,
    /// Issues of the last load, reload or validation (errors of a refused reload).
    pub last_issues: Vec<TableIssue>,
    /// Whether the startup load read the cooked payload of a pak (no source text existed, ADR-2132).
    pub cooked: bool,
}

impl TableSlot {
    /// A slot for a freshly loaded table.
    #[must_use]
    pub fn new(table: Box<dyn ErasedTable>, format: TableFormat) -> Self {
        Self {
            table,
            format,
            file: None,
            generation: 0,
            reloads: 0,
            rejected_reloads: 0,
            last_issues: Vec::new(),
            cooked: false,
        }
    }
}

/// Every data table the app holds, by asset path (`tables/weapons.table.ron`).
#[derive(Resource, Default)]
pub struct DataTables {
    slots: BTreeMap<String, TableSlot>,
}

impl DataTables {
    /// Adds (or replaces) the table at `path`.
    pub fn insert<R: Typed + Serialize + DeserializeOwned + Send + Sync + 'static>(
        &mut self,
        path: &str,
        table: DataTable<R>,
    ) {
        let format = TableFormat::of_path(path);
        self.insert_slot(path, TableSlot::new(Box::new(table), format));
    }

    /// Adds (or replaces) a slot.
    pub fn insert_slot(&mut self, path: &str, slot: TableSlot) {
        self.slots.insert(path.to_owned(), slot);
    }

    /// The typed table at `path`, when it holds rows of `R`.
    #[must_use]
    pub fn get<R: 'static>(&self, path: &str) -> Option<&DataTable<R>> {
        self.slots.get(path)?.table.as_any().downcast_ref()
    }

    /// The typed table at `path`, mutably.
    pub fn get_mut<R: 'static>(&mut self, path: &str) -> Option<&mut DataTable<R>> {
        self.slots.get_mut(path)?.table.as_any_mut().downcast_mut()
    }

    /// The slot at `path`.
    #[must_use]
    pub fn slot(&self, path: &str) -> Option<&TableSlot> {
        self.slots.get(path)
    }

    /// The slot at `path`, mutably.
    pub fn slot_mut(&mut self, path: &str) -> Option<&mut TableSlot> {
        self.slots.get_mut(path)
    }

    /// Table paths in order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.slots.keys().map(String::as_str)
    }

    /// Number of tables.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether no table is loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

type LoadFn = fn(&str, TableFormat) -> Result<Box<dyn ErasedTable>, TableLoadError>;
type EmptyFn = fn() -> Box<dyn ErasedTable>;
type CookedFn = fn(&[u8]) -> Result<Box<dyn ErasedTable>, TableLoadError>;

struct Registration {
    path: String,
    load: LoadFn,
    load_cooked: CookedFn,
    empty: EmptyFn,
}

/// Tables registered with [`DataTableAppExt::register_data_table`], loaded at startup.
#[derive(Resource, Default)]
pub struct DataTableRegistrations {
    entries: Vec<Registration>,
}

impl DataTableRegistrations {
    /// Registered asset paths in registration order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|r| r.path.as_str())
    }

    /// Registers `R` at `path` (a later registration of the same path wins).
    pub fn add<R: Typed + Serialize + DeserializeOwned + Send + Sync + 'static>(&mut self, path: &str) {
        self.entries.retain(|r| r.path != path);
        self.entries.push(Registration {
            path: path.to_owned(),
            load: load_erased::<R>,
            load_cooked: load_cooked_erased::<R>,
            empty: empty_erased::<R>,
        });
    }
}

fn load_erased<R: Typed + Serialize + DeserializeOwned + Send + Sync + 'static>(
    text: &str,
    format: TableFormat,
) -> Result<Box<dyn ErasedTable>, TableLoadError> {
    DataTable::<R>::parse(text, format).map(|t| Box::new(t) as Box<dyn ErasedTable>)
}

fn load_cooked_erased<R: Typed + Serialize + DeserializeOwned + Send + Sync + 'static>(
    bytes: &[u8],
) -> Result<Box<dyn ErasedTable>, TableLoadError> {
    DataTable::<R>::from_cooked(bytes).map(|t| Box::new(t) as Box<dyn ErasedTable>)
}

fn empty_erased<R: Typed + Serialize + DeserializeOwned + Send + Sync + 'static>() -> Box<dyn ErasedTable> {
    Box::new(DataTable::<R>::new())
}

/// Changed table files waiting for [`apply_data_table_reloads`]: `(registered path, file)`.
#[derive(Resource, Default)]
pub struct PendingTableReloads(Vec<(String, PathBuf)>);

/// Registers data tables on an [`App`].
pub trait DataTableAppExt {
    /// Loads the table at asset path `path` (`tables/weapons.table.ron`, under `assets/`) with rows
    /// of `R` at startup into [`DataTables`], and reloads it when the file changes in dev builds.
    /// `R` is registered for reflection.
    fn register_data_table<R>(&mut self, path: &str) -> &mut Self
    where
        R: Typed + GetTypeRegistration + Serialize + DeserializeOwned + Send + Sync + 'static;
}

impl DataTableAppExt for App {
    fn register_data_table<R>(&mut self, path: &str) -> &mut Self
    where
        R: Typed + GetTypeRegistration + Serialize + DeserializeOwned + Send + Sync + 'static,
    {
        self.register_type::<R>();
        if !self.world().contains_resource::<DataTableRegistrations>() {
            self.init_resource::<DataTableRegistrations>()
                .init_resource::<DataTables>()
                .init_resource::<PendingTableReloads>()
                .add_message::<SourceAssetChanged>()
                .add_systems(Startup, load_registered_data_tables)
                .add_systems(
                    Update,
                    (collect_data_table_changes, apply_data_table_reloads).chain(),
                );
        }
        self.world_mut()
            .resource_mut::<DataTableRegistrations>()
            .add::<R>(path);
        self
    }
}

/// The file behind asset path `path`, when the app has an asset server (dev builds): the
/// project's own file, else the earliest overlay root that holds it (ADR-8845).
fn table_file(world: &World, path: &str) -> Option<PathBuf> {
    let server = world.get_resource::<crate::AssetServer>()?;
    Some(server.source_path(path))
}

/// Reads asset path `path`: through the VFS when there is one (packaged games), else from the
/// asset server's project.
fn read_table_text(world: &World, path: &str) -> Result<String, String> {
    if let Some(vfs) = world.get_resource::<regia_platform::Vfs>()
        && let Ok(vfs_path) = regia_platform::VfsPath::parse(&format!("assets://{path}"))
        && let Ok(bytes) = vfs.read_sync(&vfs_path)
    {
        return String::from_utf8(bytes.to_vec()).map_err(|_| format!("{path} is not UTF-8"));
    }
    let file =
        table_file(world, path).ok_or_else(|| format!("{path}: no VFS and no asset server to read it"))?;
    std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))
}

/// The cooked payload the asset server holds for asset path `path` (a pak or the cook cache), `None`
/// when no pak or cache knows the path.
fn read_table_cooked(world: &mut World, path: &str) -> Option<Result<Vec<u8>, String>> {
    let mut server = world.get_resource_mut::<crate::AssetServer>()?;
    let id = server.id_for_path(&format!("assets/{path}"))?;
    Some(
        server
            .read_cooked(id)
            .map_err(|e| e.to_string())
            .and_then(|(file, _)| {
                let (header, payload) =
                    crate::decode_cooked_file(&file).map_err(|e| format!("{path}: {e}"))?;
                if header.type_tag != regia_core::id::TypePathHash::of(super::DATA_TABLE_COOKED_TYPE_PATH) {
                    return Err(format!("{path}: the cooked asset is not a data table"));
                }
                Ok(payload.to_vec())
            }),
    )
}

/// Where a registered table's rows come from at startup: the source text when it exists (a dev tree
/// runs what is on disk), else the cooked payload a pak holds (ADR-2132).
enum TableSource {
    Text(String),
    Cooked(Vec<u8>),
}

fn read_table_source(world: &mut World, path: &str) -> Result<TableSource, String> {
    match read_table_text(world, path) {
        Ok(text) => Ok(TableSource::Text(text)),
        Err(error) => match read_table_cooked(world, path) {
            Some(cooked) => cooked.map(TableSource::Cooked),
            None => Err(error),
        },
    }
}

/// `Startup`: loads every registered table. A table that does not load is inserted empty with its
/// errors in [`TableSlot::last_issues`] (agents report them), and the error is logged.
pub fn load_registered_data_tables(world: &mut World) {
    let Some(registrations) = world.remove_resource::<DataTableRegistrations>() else {
        return;
    };
    for registration in &registrations.entries {
        let path = registration.path.as_str();
        let format = TableFormat::of_path(path);
        let mut cooked = false;
        let loaded = read_table_source(world, path).map(|source| match source {
            TableSource::Text(text) => (registration.load)(&text, format),
            TableSource::Cooked(bytes) => {
                cooked = true;
                (registration.load_cooked)(&bytes)
            }
        });
        let mut slot = match loaded {
            Ok(loaded) => match loaded {
                Ok(table) => {
                    let issues = table.validate();
                    for issue in &issues {
                        tracing::warn!(target: "regia_asset", table = path, "{issue}");
                    }
                    let mut slot = TableSlot::new(table, format);
                    slot.last_issues = issues;
                    slot
                }
                Err(error) => {
                    tracing::error!(target: "regia_asset", table = path, "data table did not load: {error}");
                    let mut slot = TableSlot::new((registration.empty)(), format);
                    slot.last_issues = error.issues;
                    slot
                }
            },
            Err(error) => {
                tracing::error!(target: "regia_asset", table = path, "data table not read: {error}");
                let mut slot = TableSlot::new((registration.empty)(), format);
                slot.last_issues = vec![TableIssue::new(IssueCode::BadFile, error)];
                slot
            }
        };
        slot.file = table_file(world, path);
        slot.cooked = cooked;
        world.resource_mut::<DataTables>().insert_slot(path, slot);
    }
    world.insert_resource(registrations);
}

/// True when `file` is exactly `<...>/assets/<rel>`: the registered table, not a nested copy such
/// as `<...>/assets/mods/<rel>` that merely shares its tail.
fn is_asset_relative(file: &Path, rel: &str) -> bool {
    let rel = Path::new(rel);
    if !file.ends_with(rel) {
        return false;
    }
    let mut head = file;
    for _ in rel.components() {
        let Some(parent) = head.parent() else {
            return false;
        };
        head = parent;
    }
    head.file_name().is_some_and(|name| name == "assets")
}

/// `Update`: collects changed files that are registered tables.
pub fn collect_data_table_changes(
    mut changes: MessageReader<SourceAssetChanged>,
    registrations: Option<Res<DataTableRegistrations>>,
    pending: Option<ResMut<PendingTableReloads>>,
    server: Option<Res<crate::AssetServer>>,
) {
    let (Some(registrations), Some(mut pending)) = (registrations, pending) else {
        changes.clear();
        return;
    };
    for change in changes.read() {
        // A table served from an overlay root matches only while that file is the one the
        // resolver picks, so a shadowed overlay copy never replaces the project's rows (ADR-8845).
        let served_from_overlay = |rel: &str| {
            server.as_ref().is_some_and(|server| {
                server.source_roots().overlay_key(&change.path).is_some()
                    && server.source_path(rel) == change.path
            })
        };
        if let Some(registration) = registrations
            .entries
            .iter()
            .find(|r| is_asset_relative(&change.path, &r.path) || served_from_overlay(&r.path))
        {
            pending.0.push((registration.path.clone(), change.path.clone()));
        }
    }
}

/// `Update`, after [`collect_data_table_changes`]: re-reads each changed table. A file that does
/// not parse or validate keeps the old rows; its issues go to [`TableSlot::last_issues`].
pub fn apply_data_table_reloads(world: &mut World) {
    let pending = match world.get_resource_mut::<PendingTableReloads>() {
        Some(mut pending) => std::mem::take(&mut pending.0),
        None => return,
    };
    if pending.is_empty() {
        return;
    }
    let _span =
        tracing::info_span!(target: "regia_asset", "data_table_reload", files = pending.len()).entered();
    let mut done: Vec<String> = Vec::new();
    for (path, file) in pending {
        if done.contains(&path) {
            continue;
        }
        done.push(path.clone());
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(target: "regia_asset", table = %path, "cannot re-read {}: {error}", file.display());
                continue;
            }
        };
        let Some(mut tables) = world.get_resource_mut::<DataTables>() else {
            return;
        };
        let Some(slot) = tables.slot_mut(&path) else {
            continue;
        };
        match slot.table.reload(&text, slot.format) {
            Ok(warnings) => {
                slot.generation += 1;
                slot.reloads += 1;
                slot.last_issues = warnings;
                tracing::info!(target: "regia_asset", table = %path, rows = slot.table.len(), "data table reloaded");
            }
            Err(issues) => {
                slot.rejected_reloads += 1;
                for issue in &issues {
                    tracing::error!(target: "regia_asset", table = %path, "reload refused: {issue}");
                }
                slot.last_issues = issues;
            }
        }
    }
}

#[cfg(test)]
mod path_match_tests {
    use super::is_asset_relative;
    use std::path::Path;

    #[test]
    fn only_the_file_directly_under_assets_matches() {
        let rel = "tables/weapons.table.ron";
        assert!(is_asset_relative(
            Path::new("/p/assets/tables/weapons.table.ron"),
            rel
        ));
        assert!(!is_asset_relative(
            Path::new("/p/assets/mods/tables/weapons.table.ron"),
            rel
        ));
        assert!(!is_asset_relative(Path::new("tables/weapons.table.ron"), rel));
        assert!(!is_asset_relative(
            Path::new("/p/assets/tables/other.table.ron"),
            rel
        ));
    }
}
