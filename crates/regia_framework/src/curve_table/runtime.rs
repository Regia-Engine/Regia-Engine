// SPDX-License-Identifier: MPL-2.0
//! The [`CurveTables`] resource: registered tables by asset path, startup load, hot reload that
//! applies only valid files, and the rate-limited `curve_sample_nonfinite` warning.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::{AssetServer, SourceAssetChanged};

use super::{CURVE_TABLE_TYPE_PATH, CurveIssue, CurveTable, CurveTableError, TableInterp};
use crate::table_source::{TableBytes, read_table};

/// One loaded table and its bookkeeping.
#[derive(Debug, Clone, Default)]
pub struct CurveTableSlot {
    /// The table in use (empty when the first load failed).
    pub table: CurveTable,
    /// Bumped on every applied reload or edit (1 after the first load).
    pub generation: u64,
    /// Issues of the last load, reload or validation (a refused reload's errors stay here).
    pub last_issues: Vec<CurveIssue>,
    /// The source file, when the app has an asset server.
    pub file: Option<PathBuf>,
    /// Applied reloads.
    pub reloads: u32,
    /// Reloads refused because the new text did not load.
    pub rejected_reloads: u32,
    /// Whether [`Self::table`] came from a cooked payload rather than source text.
    pub cooked: bool,
}

/// Parses curve-table text by the path's extension: `.csv` as `.curves.csv` (linear), anything
/// else as `.curves.ron`.
///
/// # Errors
///
/// Every issue the parser reports.
pub fn parse_curve_table(path: &str, text: &str) -> Result<CurveTable, CurveTableError> {
    if path.ends_with(".csv") {
        CurveTable::from_csv_str(text, TableInterp::Linear)
    } else {
        CurveTable::from_ron_str(text)
    }
}

/// Every curve table the app loaded, by asset path (`tables/falloff.curves.ron`).
#[derive(Resource, Debug, Default)]
pub struct CurveTables {
    slots: BTreeMap<String, CurveTableSlot>,
}

impl CurveTables {
    /// The table at `path`.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&CurveTable> {
        self.slots.get(path).map(|slot| &slot.table)
    }

    /// The table's generation (0 when there is no table at `path`).
    #[must_use]
    pub fn generation(&self, path: &str) -> u64 {
        self.slots.get(path).map_or(0, |slot| slot.generation)
    }

    /// The slot at `path`.
    #[must_use]
    pub fn slot(&self, path: &str) -> Option<&CurveTableSlot> {
        self.slots.get(path)
    }

    /// The slot at `path`, mutably.
    pub fn slot_mut(&mut self, path: &str) -> Option<&mut CurveTableSlot> {
        self.slots.get_mut(path)
    }

    /// Every slot, sorted by path.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &CurveTableSlot)> {
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

    /// Puts `table` at `path` (replacing any table there) and returns its new generation.
    /// Handles of the previous table stay valid only when the new one names the same curves in the
    /// same order.
    pub fn insert(&mut self, path: &str, mut table: CurveTable) -> u64 {
        let slot = self.slots.entry(path.to_owned()).or_default();
        table.adopt_epoch(&slot.table);
        slot.table = table;
        slot.generation += 1;
        slot.last_issues = slot.table.validate();
        slot.generation
    }

    /// Removes the table at `path`.
    pub fn remove(&mut self, path: &str) -> Option<CurveTableSlot> {
        self.slots.remove(path)
    }

    /// Loads `text` into `path`. A text that does not load leaves the previous table in use (an
    /// empty table when there was none) and keeps the errors in
    /// [`CurveTableSlot::last_issues`].
    ///
    /// # Errors
    ///
    /// The issues of a text that did not load.
    pub fn load_text(&mut self, path: &str, text: &str) -> Result<u64, Vec<CurveIssue>> {
        self.apply_load(path, parse_curve_table(path, text), false)
    }

    /// Loads a cooked payload ([`CurveTable::to_cooked`]) into `path`, with the same rules as
    /// [`load_text`](Self::load_text).
    ///
    /// # Errors
    ///
    /// The issues of a payload that did not load.
    pub fn load_cooked(&mut self, path: &str, payload: &[u8]) -> Result<u64, Vec<CurveIssue>> {
        self.apply_load(path, CurveTable::from_cooked(payload), true)
    }

    fn apply_load(
        &mut self,
        path: &str,
        loaded: Result<CurveTable, CurveTableError>,
        cooked: bool,
    ) -> Result<u64, Vec<CurveIssue>> {
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

    /// Applies `edit` to the table at `path`; on success bumps the generation. The edit runs on a
    /// copy, so an edit of several steps (a key change, then a rename) that fails part way leaves
    /// the table as it was.
    ///
    /// # Errors
    ///
    /// `curve_table_not_found`, or the edit's own error (the table is unchanged).
    pub fn edit(
        &mut self,
        path: &str,
        edit: impl FnOnce(&mut CurveTable) -> Result<(), CurveTableError>,
    ) -> Result<u64, CurveTableError> {
        let slot = self.slots.get_mut(path).ok_or_else(|| CurveTableError {
            issues: vec![CurveIssue::error(
                "curve_table_not_found",
                format!("no curve table at `{path}`"),
            )],
        })?;
        let mut table = slot.table.clone();
        edit(&mut table)?;
        slot.table = table;
        slot.generation += 1;
        slot.last_issues = slot.table.validate();
        Ok(slot.generation)
    }
}

/// The asset paths an app registered with [`CurveTableAppExt::register_curve_table`].
#[derive(Resource, Debug, Default)]
pub struct CurveTableRegistrations {
    paths: Vec<String>,
}

impl CurveTableRegistrations {
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

/// Changed table files waiting for [`apply_curve_table_reloads`].
#[derive(Resource, Debug, Default)]
pub struct PendingCurveTableReloads(Vec<(String, PathBuf)>);

/// Registers curve tables on an [`App`].
pub trait CurveTableAppExt {
    /// Loads the table at asset path `path` (`tables/falloff.curves.ron` or `.curves.csv`, under
    /// `assets/`) into [`CurveTables`] at startup, and reloads it when the file changes (dev
    /// builds). A reload is applied only if the new text loads. Adds [`CurveTablePlugin`] when it
    /// is missing.
    fn register_curve_table(&mut self, path: &str) -> &mut Self;
}

impl CurveTableAppExt for App {
    fn register_curve_table(&mut self, path: &str) -> &mut Self {
        if !self.world().contains_resource::<CurveTables>() {
            self.add_plugins(CurveTablePlugin);
        }
        self.world_mut()
            .resource_mut::<CurveTableRegistrations>()
            .add(path);
        self
    }
}

/// Adds [`CurveTables`], the startup load of registered tables, hot reload, and the
/// `curve_sample_nonfinite` warning (once per curve per second of frame time).
pub struct CurveTablePlugin;

impl Plugin for CurveTablePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CurveTables>()
            .init_resource::<CurveTableRegistrations>()
            .init_resource::<PendingCurveTableReloads>()
            .add_message::<SourceAssetChanged>()
            .add_systems(Startup, load_registered_curve_tables)
            .add_systems(
                Update,
                (
                    collect_curve_table_changes,
                    apply_curve_table_reloads,
                    warn_nonfinite_samples,
                )
                    .chain(),
            );
    }
}

fn table_file(world: &World, path: &str) -> Option<PathBuf> {
    let server = world.get_resource::<AssetServer>()?;
    // The project file, else the earliest overlay root that holds it (ADR-8845).
    Some(server.source_path(path))
}

/// `Startup`: loads every registered table. One that does not load is inserted empty with its
/// errors in [`CurveTableSlot::last_issues`] (agents report them), and the error is logged.
pub fn load_registered_curve_tables(world: &mut World) {
    let paths: Vec<String> = match world.get_resource::<CurveTableRegistrations>() {
        Some(registrations) => registrations.paths().map(str::to_owned).collect(),
        None => return,
    };
    for path in paths {
        let read = read_table(world, &path, CURVE_TABLE_TYPE_PATH);
        let file = table_file(world, &path);
        let Some(mut tables) = world.get_resource_mut::<CurveTables>() else {
            return;
        };
        match read {
            Ok(bytes) => {
                let loaded = match &bytes {
                    TableBytes::Text(text) => tables.load_text(&path, text),
                    TableBytes::Cooked(payload) => tables.load_cooked(&path, payload),
                };
                if let Err(issues) = loaded {
                    for issue in &issues {
                        tracing::error!(target: "regia_framework", table = %path, "curve table did not load: {issue}");
                    }
                }
            }
            Err(error) => {
                tracing::error!(target: "regia_framework", table = %path, "curve table not read: {error}");
                let slot = tables.slots.entry(path.clone()).or_default();
                slot.last_issues = vec![CurveIssue::error("curve_bad_file", error)];
            }
        }
        if let Some(slot) = tables.slots.get_mut(&path) {
            slot.file = file;
        }
    }
}

/// `Update`: collects changed files that are registered tables.
pub fn collect_curve_table_changes(
    mut changes: MessageReader<SourceAssetChanged>,
    registrations: Option<Res<CurveTableRegistrations>>,
    pending: Option<ResMut<PendingCurveTableReloads>>,
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

/// `Update`, after [`collect_curve_table_changes`]: re-reads each changed table. A file that does
/// not load keeps the old table; its issues go to [`CurveTableSlot::last_issues`].
pub fn apply_curve_table_reloads(world: &mut World) {
    let pending = match world.get_resource_mut::<PendingCurveTableReloads>() {
        Some(mut pending) => std::mem::take(&mut pending.0),
        None => return,
    };
    if pending.is_empty() {
        return;
    }
    let _span =
        tracing::info_span!(target: "regia_framework", "curve_table_reload", files = pending.len()).entered();
    let mut done: Vec<String> = Vec::new();
    for (path, file) in pending {
        if done.contains(&path) {
            continue;
        }
        done.push(path.clone());
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(target: "regia_framework", table = %path, "cannot re-read {}: {error}", file.display());
                continue;
            }
        };
        let Some(mut tables) = world.get_resource_mut::<CurveTables>() else {
            return;
        };
        match tables.load_text(&path, &text) {
            Ok(generation) => {
                tracing::info!(target: "regia_framework", table = %path, generation, "curve table reloaded");
            }
            Err(issues) => {
                for issue in &issues {
                    tracing::error!(target: "regia_framework", table = %path, "reload refused: {issue}");
                }
            }
        }
    }
}

/// `Update`: once per second of frame time, logs `curve_sample_nonfinite` for each curve that was
/// sampled with a non-finite `x` since the last report.
pub fn warn_nonfinite_samples(
    time: Option<Res<Time>>,
    tables: Option<Res<CurveTables>>,
    mut since: Local<f32>,
) {
    let (Some(time), Some(tables)) = (time, tables) else {
        return;
    };
    *since += time.delta_secs();
    if *since < 1.0 {
        return;
    }
    *since = 0.0;
    for (path, slot) in tables.iter() {
        for (curve, count) in slot.table.take_nonfinite_counts() {
            tracing::warn!(
                target: "regia_framework",
                code = "curve_sample_nonfinite",
                table = path,
                curve = %curve,
                count,
                "curve sampled with a non-finite x; returned its first key value"
            );
        }
    }
}
