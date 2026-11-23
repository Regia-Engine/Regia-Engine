// SPDX-License-Identifier: MPL-2.0
//! The Localization panel (ADR-3260, `book/agents/editor-localization.md`): the project's Fluent files as
//! a key-by-locale table, read with the same `regia_ui` calls as `regia loc` (`docs/specs/content-data.md`
//! § 1.2-1.8, spec 36 § 3).
//!
//! - **Table**: one row per key, one column per locale (the source first), each cell colored by its state
//!   (translated, stale, gather placeholder, missing); filters (missing, untranslated, stale, longer than the
//!   source by a ratio), a locale picker, search and paging; per-locale progress from
//!   `regia_ui::loc_po::coverage` (the `localization.coverage` numbers).
//! - **Edit**: click a cell, edit its text, Apply. The value is written back into the file that defines it
//!   with the gather tool's atomic writer, every other byte kept; the step is on the editor's undo stack
//!   (Edit ▸ Undo restores the file's bytes).
//! - **Gather**: runs the gather pipeline of `regia loc gather` (text scan, conversations, dialogues) and
//!   writes each locale's `gathered.ftl`, as one undo step.
//! - **Find usages**: the files that name the selected key (UI layouts, scenes, conversations, dialogues,
//!   code) and, when the editor has an asset registry, the assets that reach them through the asset graph.
//! - **Pseudo**: pseudo-localization (spec § 1.8) of the viewport's runtime `Localization`, in the edit world
//!   and in play.
//! - **Export / Import** ([`exchange`], ADR-7677): a locale's PO or XLIFF file for a translator, written by the
//!   same library call as `regia loc export`; a returned file is planned first (keys added, changed and still
//!   missing, skipped entries), then written as one undo step, the same bytes as `regia loc import`.
//!
//! - **Compile** ([`compile`], ADR-7855): validates the project's locales and writes the shipping bundles with the
//!   library call of `regia loc compile`; findings come back per file and line, each a link that opens the file.
//! - **Context** (ADR-7855): each key's translator context, the `#` comment above its message, is a column of
//!   the grid and editable; it is written back as `# note:` lines (one undo step) and exports carry it (PO `#.`,
//!   XLIFF note).
//! - **Preview language** ([`preview`], ADR-7855): the toolbar's locale picker for the viewport and play.
//!
//! The panel does not edit `localization.ron` (the locale list is Project Settings' job).
//! Agent surface: `editor.loc_table`, `editor.loc_set`, `editor.loc_gather`, `editor.loc_usages`,
//! `editor.loc_pseudo`, `editor.loc_export`, `editor.loc_import`, `editor.loc_compile`,
//! `editor.loc_set_comment`, `editor.preview_locale` ([`agent`]).

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;

use crate::command::UndoStack;
use crate::data_panels::PanelError;

pub mod agent;
pub mod compile;
pub mod exchange;
pub mod model;
pub mod ops;
pub mod preview;

pub use agent::methods;
use model::{CellState, Filter, LocProject, LocTable, Query, err};
use ops::{LocFilesEdit, Usage};

#[cfg(test)]
mod tests;

/// Fill of a translated cell.
pub const TRANSLATED_FILL: egui::Color32 = egui::Color32::from_rgb(34, 70, 44);
/// Fill of a stale cell.
pub const STALE_FILL: egui::Color32 = egui::Color32::from_rgb(110, 90, 30);
/// Fill of a placeholder cell.
pub const PLACEHOLDER_FILL: egui::Color32 = egui::Color32::from_rgb(70, 60, 100);
/// Fill of a missing cell.
pub const MISSING_FILL: egui::Color32 = egui::Color32::from_rgb(110, 40, 40);
/// Fill of the selected cell.
pub const SELECTED_FILL: egui::Color32 = egui::Color32::from_rgb(46, 92, 150);

/// What the last gather did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GatherOutcome {
    /// Files the scan read.
    pub files_scanned: usize,
    /// Keys found.
    pub keys: usize,
    /// Text in a key position that is not a Fluent id: `(file, line, text)`.
    pub bad_keys: Vec<(String, u32, String)>,
    /// `(locale, path, added keys)` per written (or, dry, planned) file.
    pub files: Vec<(String, String, Vec<String>)>,
    /// Whether the files were written.
    pub written: bool,
}

/// What a translation set did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetOutcome {
    /// The file written.
    pub file: String,
    /// Whether the write created the file.
    pub created: bool,
    /// Whether the text changed anything (an identical text writes nothing and pushes no undo step).
    pub changed: bool,
    /// Validator warnings on the new text: `(code, message)`.
    pub warnings: Vec<(String, String)>,
}

/// The Localization panel's state (an edit-world resource).
#[derive(Resource)]
pub struct LocPanel {
    /// The project the cache was read from.
    root: Option<PathBuf>,
    /// The files as last read.
    project: Option<LocProject>,
    /// The table of `project`.
    table: Option<LocTable>,
    /// Read the files again before the next use (an edit, an undo, Reload).
    pub reload: bool,
    /// The shown rows.
    pub query: Query,
    /// The shown page (0-based).
    pub page: usize,
    /// Rows per page.
    pub page_size: usize,
    /// The selected cell: `(key, locale)`.
    pub selected: Option<(String, String)>,
    /// The edit box's text.
    pub edit_text: String,
    /// The last usages search: the key and what was found, and whether the asset graph was used.
    pub usages: Option<(String, Vec<Usage>, bool)>,
    /// The last gather.
    pub last_gather: Option<GatherOutcome>,
    /// The last set's warnings.
    pub warnings: Vec<(String, String)>,
    /// Pseudo-localization of the viewport.
    pub pseudo: bool,
    /// Whether the viewport's world has a runtime `Localization` (the pseudo toggle acts on it).
    pub viewport_localization: bool,
    /// The last error.
    pub error: Option<PanelError>,
    /// Export: the locale (`None`: the first target locale).
    pub exchange_locale: Option<String>,
    /// Export: the file format.
    pub exchange_format: regia_asset_pipeline::loc_exchange::ExchangeFormat,
    /// Export: which entries.
    pub exchange_only: regia_ui::loc_po::ExportFilter,
    /// Export: where it writes (empty: `.regia/loc/<locale>.<ext>`). Relative paths are under the
    /// project root. Import has its own path, [`LocPanel::import_path`], so an Export never
    /// overwrites a returned translation file the Import row names (ADR-7716).
    pub export_path: String,
    /// Import: the returned translation file it reads (empty: Import is disabled).
    pub import_path: String,
    /// The last export.
    pub last_export: Option<exchange::ExportOutcome>,
    /// A planned import waiting for Apply (its report is shown).
    pub pending_import: Option<exchange::ImportPreview>,
    /// The last applied import.
    pub last_import: Option<exchange::ImportOutcome>,
    /// The last compile.
    pub last_compile: Option<compile::CompileOutcome>,
    /// Compile: warnings fail the validation too.
    pub compile_strict: bool,
    /// The context edit box's text (the selected key's translator context, one comment line per line).
    pub context_text: String,
    /// The key `context_text` was loaded for.
    pub context_for: Option<String>,
    /// The last link click's status line.
    pub link_status: Option<String>,
}

impl Default for LocPanel {
    fn default() -> Self {
        Self {
            root: None,
            project: None,
            table: None,
            reload: true,
            query: Query::default(),
            page: 0,
            page_size: model::DEFAULT_PAGE_SIZE,
            selected: None,
            edit_text: String::new(),
            usages: None,
            last_gather: None,
            warnings: Vec::new(),
            pseudo: false,
            viewport_localization: false,
            error: None,
            exchange_locale: None,
            exchange_format: regia_asset_pipeline::loc_exchange::ExchangeFormat::Po,
            exchange_only: regia_ui::loc_po::ExportFilter::All,
            export_path: String::new(),
            import_path: String::new(),
            last_export: None,
            pending_import: None,
            last_import: None,
            last_compile: None,
            compile_strict: false,
            context_text: String::new(),
            context_for: None,
            link_status: None,
        }
    }
}

impl LocPanel {
    /// The table as last read (`None` before the first read or when the read failed).
    #[must_use]
    pub fn table(&self) -> Option<&LocTable> {
        self.table.as_ref()
    }

    /// Reads the project again when asked to, when the project changed or when the `long` ratio moved.
    fn refresh(&mut self, root: Option<&Path>) -> Result<(), PanelError> {
        let root = root.ok_or_else(|| err("no_project", "no project is open; open one first"))?;
        let ratio_moved = self
            .table
            .as_ref()
            .is_some_and(|t| t.long_ratio.to_bits() != self.query.long_ratio.to_bits());
        if self.reload || self.root.as_deref() != Some(root) || self.project.is_none() {
            self.root = Some(root.to_path_buf());
            self.project = None;
            self.table = None;
            let project = model::read_project(root)?;
            self.table = Some(model::build_table(&project, self.query.long_ratio));
            self.project = Some(project);
            self.reload = false;
        } else if ratio_moved && let Some(project) = &self.project {
            self.table = Some(model::build_table(project, self.query.long_ratio));
        }
        Ok(())
    }
}

/// Runs `f` on the panel resource after refreshing it; the error is kept on the panel.
pub(crate) fn with_panel<T>(
    world: &mut World,
    f: impl FnOnce(&mut LocPanel, &Path) -> Result<T, PanelError>,
) -> Result<T, PanelError> {
    let root = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<LocPanel>().unwrap_or_default();
    let result = panel
        .refresh(root.as_deref())
        .and_then(|()| f(&mut panel, root.as_deref().unwrap_or(Path::new("."))));
    panel.error = result.as_ref().err().cloned();
    world.insert_resource(panel);
    result
}

fn push_undo(world: &mut World, step: LocFilesEdit) {
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    stack.push(Box::new(step));
    // A translation is a discrete edit: the next one is its own step.
    stack.seal();
}

/// Sets the text of `key` in `locale` (see [`model::plan_set`]): written atomically, one undo step.
///
/// # Errors
///
/// [`model::plan_set`]'s codes, `loc_panel_no_project`, `loc_panel_io_failed`, `loc_panel_conflict`.
pub fn set_translation(
    world: &mut World,
    key: &str,
    locale: &str,
    text: &str,
) -> Result<SetOutcome, PanelError> {
    let _span = tracing::info_span!(target: "regia_editor", "loc_panel_set").entered();
    // Plan from the bytes on disk now, not from the cache.
    world.get_resource_or_insert_with(LocPanel::default).reload = true;
    let planned = with_panel(world, |panel, _| {
        let project = panel
            .project
            .as_ref()
            .ok_or_else(|| err("io_failed", "the localization files were not read"))?;
        model::plan_set(project, key, locale, text)
    })?;
    let change = planned.change;
    let created = change.before.is_none();
    let file = change.path.clone();
    if change.before.as_deref() == Some(change.after.as_str()) {
        return Ok(SetOutcome {
            file,
            created,
            changed: false,
            warnings: planned.warnings,
        });
    }
    let root =
        crate::mesh_source::project_root(world).ok_or_else(|| err("no_project", "no project is open"))?;
    let step = LocFilesEdit::write(&root, vec![change], format!("{key} ({locale})"));
    let mut panel = world.remove_resource::<LocPanel>().unwrap_or_default();
    panel.reload = true;
    panel.warnings.clone_from(&planned.warnings);
    panel.error = step.as_ref().err().cloned();
    world.insert_resource(panel);
    push_undo(world, step?);
    Ok(SetOutcome {
        file,
        created,
        changed: true,
        warnings: planned.warnings,
    })
}

/// Sets the translator context of `key` to `text` (see [`model::plan_set_comment`]): one `# note:` line per
/// line of `text` above the message in the source locale's file, atomically, one undo step. An empty `text`
/// removes the context.
///
/// # Errors
///
/// [`model::plan_set_comment`]'s codes, `loc_panel_no_project`, `loc_panel_io_failed`, `loc_panel_conflict`.
pub fn set_comment(world: &mut World, key: &str, text: &str) -> Result<SetOutcome, PanelError> {
    let _span = tracing::info_span!(target: "regia_editor", "loc_panel_set_comment").entered();
    world.get_resource_or_insert_with(LocPanel::default).reload = true;
    let change = with_panel(world, |panel, _| {
        let project = panel
            .project
            .as_ref()
            .ok_or_else(|| err("io_failed", "the localization files were not read"))?;
        model::plan_set_comment(project, key, text)
    })?;
    let created = change.before.is_none();
    let file = change.path.clone();
    if change.before.as_deref() == Some(change.after.as_str()) {
        return Ok(SetOutcome {
            file,
            created,
            changed: false,
            warnings: Vec::new(),
        });
    }
    let root =
        crate::mesh_source::project_root(world).ok_or_else(|| err("no_project", "no project is open"))?;
    let step = LocFilesEdit::write(&root, vec![change], format!("{key} (context)"));
    let mut panel = world.remove_resource::<LocPanel>().unwrap_or_default();
    panel.reload = true;
    panel.context_for = None;
    panel.error = step.as_ref().err().cloned();
    world.insert_resource(panel);
    push_undo(world, step?);
    Ok(SetOutcome {
        file,
        created,
        changed: true,
        warnings: Vec::new(),
    })
}

/// Runs the gather pipeline (`regia loc gather`) and, unless `dry_run`, writes every locale's `gathered.ftl`
/// that gains keys, as one undo step.
///
/// # Errors
///
/// `loc_panel_no_project`, `loc_panel_bad_config`, `loc_panel_io_failed`, `loc_panel_gather_failed`,
/// `loc_panel_conflict`, `loc_panel_no_locales`.
pub fn run_gather(world: &mut World, dry_run: bool) -> Result<GatherOutcome, PanelError> {
    world.get_resource_or_insert_with(LocPanel::default).reload = true;
    let (root, project) = with_panel(world, |panel, root| {
        panel
            .project
            .clone()
            .map(|p| (root.to_path_buf(), p))
            .ok_or_else(|| err("io_failed", "the localization files were not read"))
    })?;
    if project.locales.is_empty() {
        return Err(err(
            "no_locales",
            format!("no locales: create {}/<locale>/ first", model::LOCALIZATION_DIR),
        ));
    }
    let gathered = ops::gather(&root, &project)?;
    let plan = ops::plan_gather(&project, &gathered);
    let mut outcome = GatherOutcome {
        files_scanned: gathered.files_scanned,
        keys: gathered.keys.len(),
        bad_keys: gathered
            .bad_keys
            .iter()
            .map(|(at, text)| (at.file.clone(), at.line, text.clone()))
            .collect(),
        files: plan
            .iter()
            .map(|(c, added)| (c.locale.clone(), c.path.clone(), added.clone()))
            .collect(),
        written: false,
    };
    if !dry_run && !plan.is_empty() {
        let added: usize = plan.iter().map(|(_, a)| a.len()).sum();
        let changes = plan.into_iter().map(|(c, _)| c).collect();
        let step = LocFilesEdit::write(&root, changes, format!("gather ({added} entries)"));
        let mut panel = world.remove_resource::<LocPanel>().unwrap_or_default();
        panel.reload = true;
        panel.error = step.as_ref().err().cloned();
        world.insert_resource(panel);
        push_undo(world, step?);
        outcome.written = true;
    }
    let mut panel = world.remove_resource::<LocPanel>().unwrap_or_default();
    panel.last_gather = Some(outcome.clone());
    world.insert_resource(panel);
    Ok(outcome)
}

/// The usages of `key` (see [`ops::usages`]), and whether the asset graph was consulted.
///
/// # Errors
///
/// `loc_panel_bad_key`, `loc_panel_no_project`, `loc_panel_io_failed`, `loc_panel_gather_failed`.
pub fn find_usages(world: &mut World, key: &str) -> Result<(Vec<Usage>, bool), PanelError> {
    let _span = tracing::info_span!(target: "regia_editor", "loc_panel_usages").entered();
    if !regia_ui::loc_gather::is_message_id(key) {
        return Err(err("bad_key", format!("`{key}` is not a Fluent message id")));
    }
    let (root, project) = with_panel(world, |panel, root| {
        panel
            .project
            .clone()
            .map(|p| (root.to_path_buf(), p))
            .ok_or_else(|| err("io_failed", "the localization files were not read"))
    })?;
    let gathered = ops::gather(&root, &project)?;
    let graph = ops::registry_graph(world);
    let found = ops::usages(&gathered, key, graph.as_ref());
    let mut panel = world.remove_resource::<LocPanel>().unwrap_or_default();
    panel.usages = Some((key.to_owned(), found.clone(), graph.is_some()));
    world.insert_resource(panel);
    Ok((found, graph.is_some()))
}

/// Turns the viewport's pseudo-localization on or off: the edit world's runtime `Localization` now, the play
/// world's when the panel next draws (or now, given `play`). Returns whether the edit world has one.
pub fn set_pseudo(world: &mut World, play: Option<&mut World>, on: bool) -> bool {
    let has = ops::sync_pseudo(world, on);
    let in_play = play.is_some_and(|p| ops::sync_pseudo(p, on));
    let mut panel = world.get_resource_or_insert_with(LocPanel::default);
    panel.pseudo = on;
    panel.viewport_localization = has || in_play;
    has || in_play
}

// ------------------------------------------------------------------------------------------------- view

/// What a click asked for; run after the panel resource is back in the world.
enum Action {
    Set {
        key: String,
        locale: String,
        text: String,
    },
    Gather,
    Compile,
    SetContext {
        key: String,
        text: String,
    },
    OpenLink(String),
    Usages(String),
    Pseudo(bool),
    Export {
        locale: String,
        path: Option<PathBuf>,
    },
    PreviewImport(PathBuf),
    ApplyImport,
}

fn state_fill(state: CellState) -> egui::Color32 {
    match state {
        CellState::Source => egui::Color32::TRANSPARENT,
        CellState::Translated => TRANSLATED_FILL,
        CellState::Stale => STALE_FILL,
        CellState::Placeholder => PLACEHOLDER_FILL,
        CellState::Missing => MISSING_FILL,
    }
}

/// The first line of `text`, at most `max` chars, with an ellipsis when cut.
fn preview(text: &str, max: usize) -> String {
    let first = text.lines().next().unwrap_or("");
    let cut: String = first.chars().take(max).collect();
    if cut.len() < text.len() {
        format!("{cut}…")
    } else {
        cut
    }
}

/// The Localization panel. `play` is the play-in-editor world, when playing (the pseudo toggle follows it).
pub fn loc_panel(ui: &mut egui::Ui, world: &mut World, play: Option<&mut World>) {
    let _span = tracing::info_span!(target: "regia_editor", "loc_panel_draw").entered();
    let root = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<LocPanel>().unwrap_or_default();
    if let Err(e) = panel.refresh(root.as_deref()) {
        panel.error = Some(e);
    }
    // Keep the viewport's worlds on the toggle (a play session started after it was set).
    let pseudo = panel.pseudo;
    let in_edit = ops::sync_pseudo(world, pseudo);
    let in_play = play.is_some_and(|p| ops::sync_pseudo(p, pseudo));
    panel.viewport_localization = in_edit || in_play;
    let action = draw(ui, &mut panel);
    world.insert_resource(panel);
    let result = match action {
        None => Ok(()),
        Some(Action::Set { key, locale, text }) => set_translation(world, &key, &locale, &text).map(|_| ()),
        Some(Action::Gather) => run_gather(world, false).map(|_| ()),
        Some(Action::Compile) => {
            let strict = world.get_resource::<LocPanel>().is_some_and(|p| p.compile_strict);
            compile::compile_project(world, &[], strict, None).map(|_| ())
        }
        Some(Action::SetContext { key, text }) => set_comment(world, &key, &text).map(|_| ()),
        Some(Action::OpenLink(location)) => {
            if let Some(link) = crate::log_links::parse_links(&location).into_iter().next() {
                let status = crate::log_links::open_clicked(world, &link);
                if let Some(mut panel) = world.get_resource_mut::<LocPanel>() {
                    panel.link_status = Some(status);
                }
            }
            Ok(())
        }
        Some(Action::Usages(key)) => find_usages(world, &key).map(|_| ()),
        Some(Action::Pseudo(on)) => {
            set_pseudo(world, None, on);
            Ok(())
        }
        Some(Action::Export { locale, path }) => {
            let (format, only) = world
                .get_resource::<LocPanel>()
                .map(|p| (p.exchange_format, p.exchange_only))
                .unwrap_or_default();
            exchange::export_locale(world, &locale, format, only, path).map(|_| ())
        }
        Some(Action::PreviewImport(path)) => exchange::preview_import(world, &path, None).map(|_| ()),
        Some(Action::ApplyImport) => exchange::apply_import(world).map(|outcome| {
            if let Some(mut panel) = world.get_resource_mut::<LocPanel>() {
                panel.last_import = Some(outcome);
            }
        }),
    };
    if let Err(e) = result
        && let Some(mut panel) = world.get_resource_mut::<LocPanel>()
    {
        panel.error = Some(e);
    }
}

fn draw(ui: &mut egui::Ui, panel: &mut LocPanel) -> Option<Action> {
    let mut action = None;
    let detail = panel.table.as_ref().map_or_else(
        || "no localization read".to_owned(),
        |t| {
            format!(
                "source {} · {} locale(s) · {} key(s)",
                t.source,
                t.locales.len(),
                t.rows.len()
            )
        },
    );
    crate::data_panels::header(ui, "Localization", &detail);
    ui.horizontal_wrapped(|ui| {
        if ui.button("Reload").clicked() {
            panel.reload = true;
        }
        if ui
            .button("Gather")
            .on_hover_text("Find the keys content and code use and add the missing ones to each locale's gathered.ftl (regia loc gather)")
            .clicked()
        {
            action = Some(Action::Gather);
        }
        let compile = ui
            .button("Compile")
            .on_hover_text(
                "Validate the locales and write the shipping bundles to build/loc (regia loc compile); findings link to their file and line",
            );
        #[cfg(test)]
        {
            tests::record(ui, "compile", compile.rect);
            tests::record_id(ui, "compile", compile.id);
        }
        if compile.clicked() {
            action = Some(Action::Compile);
        }
        ui.checkbox(&mut panel.compile_strict, "Strict")
            .on_hover_text("Warnings fail the compile too");
        let mut pseudo = panel.pseudo;
        let label = if panel.viewport_localization {
            "Pseudo (viewport)"
        } else {
            "Pseudo (viewport: no runtime localization)"
        };
        if ui.checkbox(&mut pseudo, label).changed() {
            action = Some(Action::Pseudo(pseudo));
        }
        ui.separator();
        egui::ComboBox::from_id_salt("loc_panel_filter")
            .selected_text(panel.query.filter.as_str())
            .show_ui(ui, |ui| {
                for f in Filter::ALL {
                    if ui.selectable_value(&mut panel.query.filter, f, f.as_str()).changed() {
                        panel.page = 0;
                    }
                }
            });
        let locales = panel.table.as_ref().map(|t| t.locales.clone()).unwrap_or_default();
        egui::ComboBox::from_id_salt("loc_panel_locale")
            .selected_text(panel.query.locale.clone().unwrap_or_else(|| "every locale".to_owned()))
            .show_ui(ui, |ui| {
                if ui.selectable_value(&mut panel.query.locale, None, "every locale").changed() {
                    panel.page = 0;
                }
                for l in locales {
                    if ui.selectable_value(&mut panel.query.locale, Some(l.clone()), l).changed() {
                        panel.page = 0;
                    }
                }
            });
        if panel.query.filter == Filter::Long {
            ui.add(
                egui::DragValue::new(&mut panel.query.long_ratio)
                    .range(1.0..=4.0)
                    .speed(0.05)
                    .prefix("ratio "),
            );
        }
        ui.label("Search");
        if ui.text_edit_singleline(&mut panel.query.search).changed() {
            panel.page = 0;
        }
    });
    exchange_row(ui, panel, &mut action);
    compile_report(ui, panel, &mut action);
    if let Some(e) = &panel.error {
        e.draw(ui);
    }
    let Some(table) = panel.table.take() else {
        return action;
    };
    draw_table(ui, panel, &table, &mut action);
    panel.table = Some(table);
    action
}

fn draw_table(ui: &mut egui::Ui, panel: &mut LocPanel, table: &LocTable, action: &mut Option<Action>) {
    for c in &table.coverage {
        ui.horizontal(|ui| {
            ui.label(format!("{:>6}", c.locale));
            // Percent is 0..=100.
            let fraction = (c.percent / 100.0) as f32;
            ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_width(160.0)
                    .text(format!("{:.0} %", c.percent)),
            );
            ui.weak(format!(
                "{}/{} translated · {} placeholder · {} stale",
                c.translated, c.messages, c.placeholders, c.stale
            ));
        });
    }
    for issue in table.issues.iter().take(5) {
        ui.colored_label(
            egui::Color32::from_rgb(230, 150, 90),
            format!(
                "{} {}:{} {}",
                issue.code,
                issue.file.as_deref().unwrap_or("?"),
                issue.line.unwrap_or(0),
                issue.message
            ),
        );
    }
    ui.separator();
    let rows = table.select(&panel.query);
    let pages = rows.len().div_ceil(panel.page_size.max(1)).max(1);
    panel.page = panel.page.min(pages - 1);
    ui.horizontal(|ui| {
        if ui.add_enabled(panel.page > 0, egui::Button::new("◀")).clicked() {
            panel.page -= 1;
        }
        ui.label(format!(
            "page {} / {pages} · {} row(s)",
            panel.page + 1,
            rows.len()
        ));
        if ui
            .add_enabled(panel.page + 1 < pages, egui::Button::new("▶"))
            .clicked()
        {
            panel.page += 1;
        }
    });
    let shown: Vec<String> = table
        .locales
        .iter()
        .filter(|l| **l == table.source || panel.query.locale.as_ref().is_none_or(|q| q == *l))
        .cloned()
        .collect();
    let editor_height = 150.0;
    egui::ScrollArea::both()
        .id_salt("loc_panel_rows")
        .max_height((ui.available_height() - editor_height).max(120.0))
        .show(ui, |ui| {
            egui::Grid::new("loc_panel_grid")
                .striped(true)
                .min_col_width(80.0)
                .show(ui, |ui| {
                    ui.strong("key");
                    ui.strong("context");
                    for l in &shown {
                        ui.strong(l);
                    }
                    ui.end_row();
                    for row in rows
                        .iter()
                        .skip(panel.page * panel.page_size)
                        .take(panel.page_size)
                    {
                        ui.monospace(&row.key);
                        let context = if row.context.is_empty() {
                            "—".to_owned()
                        } else {
                            preview(&row.context.join(" · "), 40)
                        };
                        let context_button = ui.add(egui::Button::new(context).frame(false)).on_hover_text(
                            "Translator context (the comment above the message); click to edit it below",
                        );
                        #[cfg(test)]
                        tests::record(ui, &format!("context:{}", row.key), context_button.rect);
                        if context_button.clicked() {
                            panel.selected = Some((row.key.clone(), table.source.clone()));
                            panel.edit_text = row
                                .cells
                                .get(&table.source)
                                .and_then(|c| c.text.clone())
                                .unwrap_or_default();
                            panel.warnings.clear();
                        }
                        for l in &shown {
                            let Some(cell) = row.cells.get(l) else {
                                ui.label("");
                                continue;
                            };
                            let selected = panel
                                .selected
                                .as_ref()
                                .is_some_and(|(k, sl)| *k == row.key && sl == l);
                            let fill = if selected {
                                SELECTED_FILL
                            } else {
                                state_fill(cell.state)
                            };
                            let text = cell
                                .text
                                .as_deref()
                                .map_or_else(|| "—".to_owned(), |t| preview(t, 40));
                            let mut button = egui::Button::new(text).fill(fill);
                            if cell.long {
                                button = button
                                    .stroke(egui::Stroke::new(1.5, egui::Color32::from_rgb(230, 120, 60)));
                            }
                            let hover = format!(
                                "{} · {}{}",
                                cell.state.as_str(),
                                cell.file.as_deref().unwrap_or("not defined"),
                                if cell.long {
                                    " · longer than the source"
                                } else {
                                    ""
                                }
                            );
                            if ui.add(button).on_hover_text(hover).clicked() {
                                panel.selected = Some((row.key.clone(), l.clone()));
                                panel.edit_text = cell.text.clone().unwrap_or_else(|| {
                                    row.cells
                                        .get(&table.source)
                                        .and_then(|c| c.text.clone())
                                        .unwrap_or_default()
                                });
                                panel.warnings.clear();
                            }
                        }
                        ui.end_row();
                    }
                });
        });
    ui.separator();
    if let Some((key, locale)) = panel.selected.clone() {
        ui.horizontal(|ui| {
            ui.strong(format!("{key} · {locale}"));
            if let Some(src) = table
                .rows
                .iter()
                .find(|r| r.key == key)
                .and_then(|r| r.cells.get(&table.source))
                .and_then(|c| c.text.as_deref())
                && locale != table.source
            {
                ui.weak(format!("source: {}", preview(src, 80)));
            }
        });
        ui.add(
            egui::TextEdit::multiline(&mut panel.edit_text)
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        ui.horizontal(|ui| {
            if ui.button("Apply").clicked() {
                *action = Some(Action::Set {
                    key: key.clone(),
                    locale: locale.clone(),
                    text: panel.edit_text.clone(),
                });
            }
            if ui.button("Find usages").clicked() {
                *action = Some(Action::Usages(key.clone()));
            }
        });
        for (code, message) in &panel.warnings {
            ui.colored_label(
                egui::Color32::from_rgb(230, 190, 90),
                crate::data_panels::sentence(message),
            )
            .on_hover_text(format!("Agent error code: {code}"));
        }
        if panel.context_for.as_deref() != Some(key.as_str()) {
            panel.context_text = table
                .rows
                .iter()
                .find(|r| r.key == key)
                .map(|r| r.context.join("\n"))
                .unwrap_or_default();
            panel.context_for = Some(key.clone());
        }
        ui.label("Context for translators")
            .on_hover_text("The comment above the message, one line per line; written as # note: lines in the source locale's file");
        let r = ui.add(
            egui::TextEdit::multiline(&mut panel.context_text)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .hint_text("where the text appears, what the variables mean"),
        );
        #[cfg(test)]
        tests::record(ui, "context-edit", r.rect);
        let _ = r;
        let r = ui.button("Apply context");
        #[cfg(test)]
        {
            tests::record(ui, "context-apply", r.rect);
            tests::record_id(ui, "context-apply", r.id);
        }
        if r.clicked() {
            *action = Some(Action::SetContext {
                key: key.clone(),
                text: panel.context_text.clone(),
            });
        }
    }
    if let Some((key, usages, graph)) = &panel.usages {
        ui.label(format!(
            "{} usage(s) of {key}{}",
            usages.len(),
            if *graph { " (with the asset graph)" } else { "" }
        ));
        for u in usages {
            match &u.via {
                None => ui.monospace(format!("{}:{} ({})", u.file, u.line, u.kind)),
                Some(via) => ui.monospace(format!("{} ({}) via {via}", u.file, u.kind)),
            };
        }
    }
    if let Some(g) = &panel.last_gather {
        let added: usize = g.files.iter().map(|(_, _, a)| a.len()).sum();
        ui.weak(format!(
            "gather: {} key(s) in {} file(s); {added} entr(ies) added to {} file(s); {} bad key(s)",
            g.keys,
            g.files_scanned,
            g.files.len(),
            g.bad_keys.len()
        ));
    }
}

/// The last compile's result: the verdict, the bundles written, and each finding with a link that opens its
/// file at the line (ADR-7855).
fn compile_report(ui: &mut egui::Ui, panel: &mut LocPanel, action: &mut Option<Action>) {
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let Some(c) = &panel.last_compile else {
        return;
    };
    if c.ok {
        ui.colored_label(
            tokens.accents.ok,
            format!(
                "compiled {} locale(s) to {}: {} ({} warning(s))",
                c.locales.len(),
                c.dir
                    .as_ref()
                    .map_or_else(String::new, |d| d.display().to_string()),
                c.locales.join(", "),
                c.warnings
            ),
        );
    } else {
        ui.colored_label(
            tokens.accents.error_text,
            format!(
                "compile failed: {} error(s), {} warning(s){}; nothing was written",
                c.errors,
                c.warnings,
                if c.strict { " (strict)" } else { "" }
            ),
        );
    }
    egui::ScrollArea::vertical()
        .id_salt("loc_panel_compile_findings")
        .max_height(120.0)
        .show(ui, |ui| {
            for f in c.findings.iter().filter(|f| f.severity != "info").take(50) {
                ui.horizontal_wrapped(|ui| {
                    let color = if f.severity == "error" {
                        tokens.accents.error_text
                    } else {
                        tokens.accents.unsaved
                    };
                    ui.colored_label(color, format!("{} {}", f.severity, f.code));
                    if let Some(location) = f.location() {
                        let r = ui.link(&location).on_hover_text("Open the file at this line");
                        #[cfg(test)]
                        tests::record(ui, &format!("finding-link:{location}"), r.rect);
                        if r.clicked() {
                            *action = Some(Action::OpenLink(location));
                        }
                    }
                    ui.label(format!("{}: {}", f.locale, f.message));
                });
            }
        });
    if let Some(status) = &panel.link_status {
        ui.weak(status);
    }
}

/// The project-relative or absolute file `text` names (`None`: empty).
fn exchange_file(panel: &LocPanel, text: &str) -> Option<PathBuf> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let path = PathBuf::from(text);
    Some(match &panel.root {
        Some(root) if path.is_relative() => root.join(path),
        _ => path,
    })
}

/// Export and Import (ADR-7677): locale, format and filter pickers, the exchange file with a picker, the
/// buttons, and the pending import's report with Apply and Cancel.
fn exchange_row(ui: &mut egui::Ui, panel: &mut LocPanel, action: &mut Option<Action>) {
    use regia_asset_pipeline::loc_exchange::ExchangeFormat;
    use regia_ui::loc_po::ExportFilter;
    let targets: Vec<String> = panel
        .table
        .as_ref()
        .map(|t| t.locales.iter().filter(|l| **l != t.source).cloned().collect())
        .unwrap_or_default();
    let locale = panel
        .exchange_locale
        .clone()
        .filter(|l| targets.contains(l))
        .or_else(|| targets.first().cloned());
    ui.horizontal_wrapped(|ui| {
        ui.label("Exchange");
        let combo = egui::ComboBox::from_id_salt("loc_panel_exchange_locale")
            .selected_text(locale.clone().unwrap_or_else(|| "no target locale".to_owned()))
            .show_ui(ui, |ui| {
                for l in &targets {
                    let r = ui.selectable_label(locale.as_ref() == Some(l), l);
                    #[cfg(test)]
                    tests::record(ui, &format!("exchange-locale:{l}"), r.rect);
                    if r.clicked() {
                        panel.exchange_locale = Some(l.clone());
                    }
                }
            });
        #[cfg(test)]
        tests::record(ui, "exchange-locale", combo.response.rect);
        let _ = combo;
        let combo = egui::ComboBox::from_id_salt("loc_panel_exchange_format")
            .selected_text(exchange::format_name(panel.exchange_format))
            .show_ui(ui, |ui| {
                for f in [ExchangeFormat::Po, ExchangeFormat::Xliff] {
                    let r = ui.selectable_value(&mut panel.exchange_format, f, exchange::format_name(f));
                    #[cfg(test)]
                    tests::record(ui, &format!("exchange-format:{}", exchange::format_name(f)), r.rect);
                    let _ = r;
                }
            });
        #[cfg(test)]
        tests::record(ui, "exchange-format", combo.response.rect);
        let _ = combo;
        egui::ComboBox::from_id_salt("loc_panel_exchange_only")
            .selected_text(exchange::filter_name(panel.exchange_only))
            .show_ui(ui, |ui| {
                for f in [ExportFilter::All, ExportFilter::Untranslated, ExportFilter::Stale] {
                    ui.selectable_value(&mut panel.exchange_only, f, exchange::filter_name(f));
                }
            });
        let r = ui.add(
            egui::TextEdit::singleline(&mut panel.export_path)
                // A short hint that fits the field; the default path on hover (bug 19, ADR-8586).
                .hint_text("Export file")
                .desired_width(200.0),
        )
        .on_hover_text("Where Export writes; empty: .regia/loc/<locale>.po");
        #[cfg(test)]
        tests::record(ui, "exchange-export-path", r.rect);
        let _ = r;
        if ui
            .button("…")
            .on_hover_text("Pick where Export writes the PO or XLIFF file")
            .clicked()
        {
            let dialog = crate::file_dialog::FileDialog::new().add_filter("PO / XLIFF", &exchange::EXCHANGE_EXTENSIONS);
            let dialog = match &panel.root {
                Some(root) => dialog.set_directory(root),
                None => dialog,
            };
            if let Some(path) = dialog.save_file() {
                panel.export_path = path.display().to_string();
            }
        }
        let r = ui
            .add_enabled(locale.is_some(), egui::Button::new("Export"))
            .on_hover_text("Write the locale's PO or XLIFF file for a translator (regia loc export)");
        #[cfg(test)]
        tests::record(ui, "exchange-export", r.rect);
        if r.clicked()
            && let Some(locale) = &locale
        {
            *action = Some(Action::Export {
                locale: locale.clone(),
                path: exchange_file(panel, &panel.export_path),
            });
        }
        let r = ui.add(
            egui::TextEdit::singleline(&mut panel.import_path)
                .hint_text("returned file to import")
                .desired_width(200.0),
        );
        #[cfg(test)]
        tests::record(ui, "exchange-import-path", r.rect);
        let _ = r;
        if ui
            .button("…")
            .on_hover_text("Pick the returned translation file (PO or XLIFF) to import")
            .clicked()
        {
            let dialog = crate::file_dialog::FileDialog::new().add_filter("PO / XLIFF", &exchange::EXCHANGE_EXTENSIONS);
            let dialog = match &panel.root {
                Some(root) => dialog.set_directory(root),
                None => dialog,
            };
            if let Some(path) = dialog.pick_file() {
                panel.import_path = path.display().to_string();
            }
        }
        let file = exchange_file(panel, &panel.import_path);
        let r = ui
            .add_enabled(file.is_some(), egui::Button::new("Import…"))
            .on_hover_text("Read a returned PO or XLIFF file: shows what changes, then Apply writes it (regia loc import)");
        #[cfg(test)]
        tests::record(ui, "exchange-import", r.rect);
        if r.clicked()
            && let Some(file) = file
        {
            *action = Some(Action::PreviewImport(file));
        }
    });
    if let Some(e) = &panel.last_export {
        ui.weak(format!(
            "exported {} entr(ies) of {} to {}",
            e.entries,
            e.locale,
            e.file.display()
        ));
    }
    let mut cancel = false;
    if let Some(p) = &panel.pending_import {
        ui.group(|ui| {
            ui.strong(format!(
                "Import {} ({}) into {}",
                p.file.display(),
                exchange::format_name(p.format),
                p.locale
            ));
            if let Some((code, message)) = &p.refused {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 110, 90),
                    format!("Refused: {message}"),
                )
                .on_hover_text(format!("Agent error code: {code}"));
            }
            let list = |keys: &[String]| {
                if keys.is_empty() {
                    "none".to_owned()
                } else {
                    keys.join(", ")
                }
            };
            ui.label(format!("added ({}): {}", p.added.len(), list(&p.added)));
            ui.label(format!("changed ({}): {}", p.changed.len(), list(&p.changed)));
            ui.label(format!("missing ({}): {}", p.missing.len(), list(&p.missing)));
            for (code, key, message) in p.skipped.iter().chain(&p.warnings) {
                ui.colored_label(egui::Color32::from_rgb(230, 190, 90), format!("{key}: {message}"))
                    .on_hover_text(format!("Agent error code: {code}"));
            }
            ui.horizontal(|ui| {
                let r = ui.add_enabled(
                    p.refused.is_none() && !p.changes.is_empty(),
                    egui::Button::new(format!("Apply ({} file(s))", p.changes.len())),
                );
                #[cfg(test)]
                tests::record(ui, "exchange-apply", r.rect);
                if r.clicked() {
                    *action = Some(Action::ApplyImport);
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
    }
    if cancel {
        panel.pending_import = None;
    }
    if let Some(i) = &panel.last_import {
        ui.weak(format!(
            "imported {} entr(ies) for {}{}",
            i.preview.imported,
            i.preview.locale,
            if i.written { "" } else { " (nothing changed)" }
        ));
    }
}
