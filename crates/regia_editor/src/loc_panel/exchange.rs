// SPDX-License-Identifier: MPL-2.0
//! The Localization panel's translator exchange (ADR-7677, spec 36 § 3): **Export** writes a locale's
//! PO or XLIFF file and **Import** reads one back, through the same library functions as `regia loc export`
//! and `regia loc import` (`regia_asset_pipeline::loc_exchange`), so the panel, the agent methods and the CLI
//! write the same bytes.
//!
//! An import is two steps: [`preview_import`] plans it and reports which keys it adds, changes and still
//! misses, writing nothing; [`apply_import`] writes the planned files (`imported.ftl`, and `gathered.ftl` when
//! placeholders were removed) as one editor undo step.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_asset_pipeline::loc_exchange::{self as exchange, ExchangeFile, ExchangeFormat, PlannedImport};
use regia_ecs::prelude::*;
use regia_ui::loc_po::ExportFilter;

use super::model::{self, CellState, FileChange, LocProject, LocTable, err};
use super::ops::LocFilesEdit;
use super::{LocPanel, push_undo};
use crate::data_panels::PanelError;

/// The exchange file types the panel's picker offers.
pub const EXCHANGE_EXTENSIONS: [&str; 3] = ["po", "xlf", "xliff"];

/// The name of an export filter (`all`, `untranslated`, `stale`).
#[must_use]
pub fn filter_name(filter: ExportFilter) -> &'static str {
    match filter {
        ExportFilter::All => "all",
        ExportFilter::Untranslated => "untranslated",
        ExportFilter::Stale => "stale",
    }
}

/// Parses an export filter name.
///
/// # Errors
///
/// `loc_panel_bad_filter` for another name.
pub fn parse_filter(name: &str) -> Result<ExportFilter, PanelError> {
    match name {
        "all" => Ok(ExportFilter::All),
        "untranslated" => Ok(ExportFilter::Untranslated),
        "stale" => Ok(ExportFilter::Stale),
        _ => Err(err(
            "bad_filter",
            format!("no export filter `{name}`; filters: all, untranslated, stale"),
        )),
    }
}

/// The name of an exchange format (`po`, `xliff`).
#[must_use]
pub fn format_name(format: ExchangeFormat) -> &'static str {
    match format {
        ExchangeFormat::Po => "po",
        ExchangeFormat::Xliff => "xliff",
    }
}

/// Parses an exchange format name (`po`, `xliff` or `xlf`).
///
/// # Errors
///
/// `loc_panel_bad_format` for another name.
pub fn parse_format(name: &str) -> Result<ExchangeFormat, PanelError> {
    ExchangeFormat::parse(name).ok_or_else(|| {
        err(
            "bad_format",
            format!("no exchange format `{name}`; formats: po, xliff"),
        )
    })
}

/// What an export wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportOutcome {
    /// The file written.
    pub file: PathBuf,
    /// The locale exported.
    pub locale: String,
    /// The format.
    pub format: ExchangeFormat,
    /// Entries in the file.
    pub entries: usize,
    /// Messages with attributes (attributes are not exported).
    pub skipped_attributes: usize,
    /// Fluent terms (not exported).
    pub skipped_terms: usize,
    /// Entries the translation memory pre-filled.
    pub tm_pre_filled: usize,
    /// Near-match notes the translation memory added.
    pub tm_near: usize,
}

/// One note of an import report: `(code, key, message)`.
pub type ImportNote = (String, String, String);

/// A planned import, before anything is written: the library's report plus the keys it adds, changes and
/// still misses, and the files it would write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportPreview {
    /// The file read.
    pub file: PathBuf,
    /// The format detected from its content.
    pub format: ExchangeFormat,
    /// The locale imported.
    pub locale: String,
    /// Entries the import accepts (as `regia loc import` counts them).
    pub imported: usize,
    /// Entries skipped, with the reason.
    pub skipped: Vec<ImportNote>,
    /// Entries accepted with a warning.
    pub warnings: Vec<ImportNote>,
    /// The file was refused: `(code, message)`; nothing would be written.
    pub refused: Option<(String, String)>,
    /// Keys the locale had no real translation for (missing, or only a gather placeholder) that the import
    /// translates.
    pub added: Vec<String>,
    /// Keys whose translation the import changes.
    pub changed: Vec<String>,
    /// Source keys the locale still has no real translation for after the import.
    pub missing: Vec<String>,
    /// The files the import rewrites (empty when it writes nothing).
    pub changes: Vec<FileChange>,
}

/// What an applied import did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportOutcome {
    /// The preview it applied.
    pub preview: ImportPreview,
    /// Whether files were written (and one undo step pushed).
    pub written: bool,
}

fn project_root(world: &World) -> Result<PathBuf, PanelError> {
    crate::mesh_source::project_root(world)
        .ok_or_else(|| err("no_project", "no project is open; open one first"))
}

/// Exports `locale` as `format` (`regia loc export`) to `out`, or to `<project>/.regia/loc/<locale>.po`
/// (`.xlf`) when `out` is `None`.
///
/// # Errors
///
/// `loc_panel_no_project`, `loc_panel_export_failed` (the localization cannot be read, the locale is the
/// source locale), `loc_panel_io_failed` (the file cannot be written).
pub fn export_locale(
    world: &mut World,
    locale: &str,
    format: ExchangeFormat,
    filter: ExportFilter,
    out: Option<PathBuf>,
) -> Result<ExportOutcome, PanelError> {
    let _span = tracing::info_span!(target: "regia_editor", "loc_panel_export").entered();
    let root = project_root(world)?;
    if !exchange::is_locale_tag(locale) {
        return Err(err("bad_locale", format!("`{locale}` is not a locale tag")));
    }
    let exported = exchange::export(&root, locale, format, filter)
        .map_err(|e| err("export_failed", format!("{e:#}")))?;
    let file = out.unwrap_or_else(|| exchange::default_export_path(&root, locale, format));
    exchange::write_atomic(&file, &exported.text).map_err(|e| err("io_failed", format!("{e:#}")))?;
    let outcome = ExportOutcome {
        file,
        locale: locale.to_owned(),
        format,
        entries: exported.entries,
        skipped_attributes: exported.skipped_attributes,
        skipped_terms: exported.skipped_terms,
        tm_pre_filled: exported.tm_pre_filled,
        tm_near: exported.tm_near,
    };
    let mut panel = world.get_resource_or_insert_with(LocPanel::default);
    panel.last_export = Some(outcome.clone());
    Ok(outcome)
}

/// `project` with the planned files of `planned` in place (a new file joins its locale in runtime order).
fn after_import(project: &LocProject, planned: &PlannedImport) -> LocProject {
    let mut after = project.clone();
    let locale = match after.locales.iter().position(|l| l.locale == planned.locale) {
        Some(i) => i,
        None => {
            after.locales.push(model::LocaleFiles {
                locale: planned.locale.clone(),
                files: Vec::new(),
            });
            after.locales.sort_by(|a, b| a.locale.cmp(&b.locale));
            after
                .locales
                .iter()
                .position(|l| l.locale == planned.locale)
                .unwrap_or_default()
        }
    };
    let files = &mut after.locales[locale].files;
    for file in planned.files() {
        match files.iter_mut().find(|(path, _)| *path == file.path) {
            Some(entry) => entry.1 = file.text,
            None => files.push((file.path, file.text)),
        }
    }
    // Runtime order: sorted by file name (the paths share the locale directory).
    files.sort_by(|a, b| a.0.cmp(&b.0));
    after
}

/// The real translation of each key of `locale` in `table` (stale ones count: they are real text).
fn translations(table: &LocTable, locale: &str) -> BTreeMap<String, Option<String>> {
    table
        .rows
        .iter()
        .map(|row| {
            let text = row
                .cells
                .get(locale)
                .filter(|c| matches!(c.state, CellState::Translated | CellState::Stale))
                .and_then(|c| c.text.clone());
            (row.key.clone(), text)
        })
        .collect()
}

/// The keys an import adds, changes and still misses: the table of the project before and after the
/// planned files, for the import's locale.
fn key_report(before: &LocProject, planned: &PlannedImport) -> (Vec<String>, Vec<String>, Vec<String>) {
    let after = after_import(before, planned);
    let old = translations(
        &model::build_table(before, model::DEFAULT_LONG_RATIO),
        &planned.locale,
    );
    let table = model::build_table(&after, model::DEFAULT_LONG_RATIO);
    let new = translations(&table, &planned.locale);
    let (mut added, mut changed) = (Vec::new(), Vec::new());
    for (key, text) in &new {
        let Some(text) = text else { continue };
        match old.get(key).cloned().flatten() {
            None => added.push(key.clone()),
            Some(old) if old != *text => changed.push(key.clone()),
            Some(_) => {}
        }
    }
    let missing = table
        .rows
        .iter()
        .filter(|row| row.in_source && new.get(&row.key).is_none_or(Option::is_none))
        .map(|row| row.key.clone())
        .collect();
    (added, changed, missing)
}

/// Plans the import of the PO or XLIFF file at `file` (`regia loc import --dry-run`) and keeps the preview on
/// the panel; writes nothing. `locale` overrides the file's `Language`.
///
/// # Errors
///
/// `loc_panel_no_project`, `loc_panel_io_failed` (the file cannot be read), `loc_panel_import_failed` (the
/// file is not PO or XLIFF, no locale is known, the localization cannot be read).
pub fn preview_import(
    world: &mut World,
    file: &Path,
    locale: Option<String>,
) -> Result<ImportPreview, PanelError> {
    let _span = tracing::info_span!(target: "regia_editor", "loc_panel_import_preview").entered();
    let root = project_root(world)?;
    let text =
        std::fs::read_to_string(file).map_err(|e| err("io_failed", format!("{}: {e}", file.display())))?;
    let parsed = ExchangeFile::parse(&text, &file.display().to_string())
        .map_err(|e| err("import_failed", format!("{e:#}")))?;
    let planned = exchange::plan_import(&root, parsed.language(), locale, |ctx| parsed.plan(ctx))
        .map_err(|e| err("import_failed", format!("{e:#}")))?;
    let before = model::read_project(&root)?;
    let (added, changed, missing) = key_report(&before, &planned);
    let current = |path: &str| {
        before
            .files(&planned.locale)
            .and_then(|files| files.iter().find(|(p, _)| p == path))
            .map(|(_, text)| text.clone())
    };
    let changes = planned
        .files()
        .into_iter()
        .map(|f| FileChange {
            before: current(&f.path),
            path: f.path,
            locale: f.locale,
            after: f.text,
        })
        .filter(|c| c.before.as_deref() != Some(c.after.as_str()))
        .collect();
    let notes = |ns: &[regia_ui::loc_po::ImportNote]| {
        ns.iter()
            .map(|n| (n.code.clone(), n.key.clone(), n.message.clone()))
            .collect()
    };
    let report = &planned.plan.report;
    let preview = ImportPreview {
        file: file.to_path_buf(),
        format: parsed.format(),
        locale: planned.locale.clone(),
        imported: report.imported,
        skipped: notes(&report.skipped),
        warnings: notes(&report.warnings),
        refused: report
            .refused
            .as_ref()
            .map(|n| (n.code.clone(), n.message.clone())),
        added,
        changed,
        missing,
        changes,
    };
    let mut panel = world.get_resource_or_insert_with(LocPanel::default);
    panel.pending_import = Some(preview.clone());
    Ok(preview)
}

/// Writes the panel's pending import ([`preview_import`]) as one undo step; a refused file or a plan that
/// changes nothing writes nothing. The pending import is cleared either way.
///
/// # Errors
///
/// `loc_panel_no_import` (no preview), `loc_panel_import_refused` (the file was refused),
/// `loc_panel_no_project`, `loc_panel_conflict` (a file changed on disk since the preview),
/// `loc_panel_io_failed`.
pub fn apply_import(world: &mut World) -> Result<ImportOutcome, PanelError> {
    let _span = tracing::info_span!(target: "regia_editor", "loc_panel_import_apply").entered();
    let preview = world
        .get_resource_mut::<LocPanel>()
        .and_then(|mut p| p.pending_import.take())
        .ok_or_else(|| {
            err(
                "no_import",
                "no import is planned; pick a file and press Import first",
            )
        })?;
    if let Some((code, message)) = &preview.refused {
        return Err(err("import_refused", format!("{message} ({code})")));
    }
    if preview.changes.is_empty() {
        return Ok(ImportOutcome {
            preview,
            written: false,
        });
    }
    let root = project_root(world)?;
    let step = LocFilesEdit::write(
        &root,
        preview.changes.clone(),
        format!("import {} ({} entries)", preview.locale, preview.imported),
    );
    let mut panel = world.remove_resource::<LocPanel>().unwrap_or_default();
    panel.reload = true;
    panel.error = step.as_ref().err().cloned();
    world.insert_resource(panel);
    push_undo(world, step?);
    Ok(ImportOutcome {
        preview,
        written: true,
    })
}
