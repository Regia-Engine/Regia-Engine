// SPDX-License-Identifier: MPL-2.0
//! The Tables panel (`docs/specs/51-curve-tables-and-choosers.md` § 6, ADR-2031): curve tables and choosers
//! opened from the project, edited with the same library calls as `curves.*` and `chooser.*`, saved
//! atomically. Agent surface: `editor.tables_panel_*` (`book/agents/editor-data-panels.md`).
//!
//! - **Curves**: the list of curves with add, rename and delete, a graph (every curve overlaid,
//!   the selected one with its keys: drag a key, double-click to add one, right-drag or
//!   middle-drag to pan, wheel to zoom), per-curve interpolation and extrapolation, a key grid
//!   and a CSV paste box.
//! - **Chooser**: one grid row per chooser row and one column per chooser column, a **test context**
//!   strip and a seed. Every change re-runs `ChooserTable::evaluate_traced` with full detail, and
//!   the grid colors each cell by its verdict (Pass, Fail, Score, Skipped) and marks the selected
//!   row. Rows reorder with the arrows, are added and removed with the bar under the grid, and
//!   their result edits inline as RON; columns toggle, are added (bound to a declared input or
//!   output) and removed from the header; the options bar sets the mode, result type, catch-all,
//!   near-best band and recency. All of these run the same edit functions the agent's
//!   `chooser.set_row` / `remove_row` / `set_column` / `remove_column` / `set_options` run.
//!   Validation issues badge the offending cell.
//!
//! - **Data** (ADR-2132): a typed data table (`.table.ron`, `.table.csv`) as a grid of keys and fields with
//!   its schema errors badged, edits that would add an error refused, row insert, remove, rename and
//!   reorder ([`data`]).
//!
//! Edits never write the file: `Save` (`editor.tables_panel_save`) does. Each edit can be undone
//! (`editor.tables_panel_undo`, 32 steps per open table). Paths are relative to the project's `assets/`
//! directory, as in `curves.*` and `chooser.*`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::Rng;
use regia_ecs::prelude::*;
use regia_framework::chooser::{
    Cell, ChooserContext, ChooserIssue, ChooserOutcome, ChooserTable, ChooserTrace, ColumnClass, MAX_DEPTH,
    Mode, ResultEntry, RowOutcome, TraceDetail, Verdict, cell_ron, parse_cell, parse_column_kind,
    parse_value, result_ron, value_ron,
};
use regia_framework::curve_table::{CurveIssue, CurveKey, CurveTable, Extrap, NamedCurve, TableInterp};
use serde_json::{Value, json};

use crate::anim::view2d::View2d;
use crate::data_panels::{PanelError, confined, from_rpc, opt_bool, opt_f32, str_param, write_atomic};
use crate::viewport_agent::Handler;

const HINT: &str = "see book/agents/editor-data-panels.md";
const PREFIX: &str = "tables_panel";
/// Undo steps kept per open table.
pub const UNDO_DEPTH: usize = 32;
const CURVE_SUFFIXES: &[&str] = &[".curves.ron", ".curves.csv"];
const CHOOSER_SUFFIXES: &[&str] = &[".chooser.ron"];

pub(crate) mod autosave;
pub mod data;
pub mod exchange;
use data::{DATA_SUFFIXES, SchemaLookup};
pub use data::{DataDoc, ERROR_FILL, SchemaSource, WARNING_FILL};

/// Fill of a cell whose gate passed.
pub const PASS_FILL: egui::Color32 = egui::Color32::from_rgb(38, 104, 52);
/// Fill of a cell whose gate failed.
pub const FAIL_FILL: egui::Color32 = egui::Color32::from_rgb(122, 40, 40);
/// Fill of a rank cell that added a cost.
pub const SCORE_FILL: egui::Color32 = egui::Color32::from_rgb(112, 96, 34);
/// Fill of the selected row's id cell.
pub const SELECTED_FILL: egui::Color32 = egui::Color32::from_rgb(46, 92, 150);

/// Curve colors in the graph, by curve index (the selected curve is drawn thicker).
const CURVE_COLORS: [egui::Color32; 6] = [
    egui::Color32::from_rgb(235, 110, 100),
    egui::Color32::from_rgb(120, 220, 120),
    egui::Color32::from_rgb(110, 160, 255),
    egui::Color32::from_rgb(230, 200, 90),
    egui::Color32::from_rgb(200, 130, 230),
    egui::Color32::from_rgb(100, 220, 220),
];

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(&format!("{PREFIX}_{code}"), message)
}

/// Which tab the panel shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TablesTab {
    /// Curve tables.
    #[default]
    Curves,
    /// Choosers.
    Chooser,
    /// Typed data tables (ADR-2132).
    Data,
}

impl TablesTab {
    /// `curves` or `chooser`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Curves => "curves",
            Self::Data => "data",
            Self::Chooser => "chooser",
        }
    }
}

/// An open curve table.
pub struct CurveDoc {
    /// Path under `assets/`.
    pub path: String,
    /// The table being edited.
    pub table: CurveTable,
    /// The selected curve.
    pub selected: Option<String>,
    /// The selected key of the selected curve.
    pub selected_key: Option<usize>,
    /// Bumped by every applied edit.
    pub generation: u64,
    /// Whether there are unsaved edits.
    pub dirty: bool,
    /// The table's validation issues.
    pub issues: Vec<CurveIssue>,
    /// The graph's view.
    pub view: View2d,
    /// The CSV paste box.
    pub csv: String,
    undo: Vec<CurveTable>,
    history_id: u64,
    fitted: bool,
    last_gesture: Option<u64>,
    gestures: u64,
    dragging: Option<CurveDrag>,
    /// UI scratch: the add and rename box.
    name_input: String,
}

/// What a graph drag holds: a key, or one of the selected key's tangent handles.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CurveDrag {
    Key(usize),
    Handle { key: usize, out: bool },
}

/// The slope the graph draws for `key` of `curve` on `side`: its explicit tangent, else the sampled slope
/// around it (a central difference a hundredth of the neighbouring span wide, one-sided at the ends).
#[must_use]
pub fn display_slope(table: &CurveTable, curve: &NamedCurve, key: usize, out: bool) -> f32 {
    let Some(k) = curve.keys.get(key) else {
        return 0.0;
    };
    if let Some(t) = if out { k.out_tangent } else { k.in_tangent } {
        return t;
    }
    let span = if out {
        curve.keys.get(key + 1).map(|n| n.x - k.x)
    } else {
        key.checked_sub(1)
            .and_then(|i| curve.keys.get(i))
            .map(|p| k.x - p.x)
    };
    let Some(span) = span.filter(|s| *s > 0.0) else {
        return 0.0;
    };
    let h = span * 0.01;
    let (a, b) = if out { (k.x, k.x + h) } else { (k.x - h, k.x) };
    match (
        table.sample_named(&curve.name, a),
        table.sample_named(&curve.name, b),
    ) {
        (Some(ya), Some(yb)) => (yb - ya) / (b - a),
        _ => 0.0,
    }
}

/// Most x positions the curve grid shows.
pub const GRID_COLUMNS: usize = 256;

/// The curve grid's columns: every key position of every curve, sorted, without repeats (the union axis of
/// the CSV form), at most [`GRID_COLUMNS`].
#[must_use]
pub fn grid_axis(table: &CurveTable) -> Vec<f32> {
    let mut axis: Vec<f32> = table
        .curves()
        .iter()
        .flat_map(|c| c.keys.iter().map(|k| k.x))
        .collect();
    axis.sort_by(f32::total_cmp);
    axis.dedup();
    axis.truncate(GRID_COLUMNS);
    axis
}

/// What the grid shows for one cell after the last evaluation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CellState {
    /// The gate passed.
    Pass,
    /// The gate failed (or the distance dropped the row).
    Fail,
    /// The rank column added this cost.
    Score(f32),
    /// Tested by no one (a disabled column, or the row was not evaluated).
    Skipped,
}

impl CellState {
    /// `pass`, `fail`, `score` or `skipped`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Score(_) => "score",
            Self::Skipped => "skipped",
        }
    }

    /// The grid's fill for this state.
    #[must_use]
    pub fn fill(self) -> egui::Color32 {
        match self {
            Self::Pass => PASS_FILL,
            Self::Fail => FAIL_FILL,
            Self::Score(_) => SCORE_FILL,
            Self::Skipped => egui::Color32::TRANSPARENT,
        }
    }
}

/// An open chooser with its test context and last evaluation.
pub struct ChooserDoc {
    /// Path under `assets/`.
    pub path: String,
    /// The table being edited.
    pub table: ChooserTable,
    /// Tables it references (`Chooser("...")` results), read from the project when it opened.
    pub nested: BTreeMap<String, ChooserTable>,
    /// The test context.
    pub context: ChooserContext,
    /// The seed of the weighted draw (`Rng::new(seed)` per evaluation).
    pub seed: u64,
    /// The last evaluation's answer.
    pub outcome: Option<ChooserOutcome>,
    /// The last evaluation's full trace.
    pub trace: ChooserTrace,
    /// Bumped by every applied edit.
    pub generation: u64,
    /// Whether there are unsaved edits.
    pub dirty: bool,
    /// The table's validation issues.
    pub issues: Vec<ChooserIssue>,
    undo: Vec<ChooserTable>,
    history_id: u64,
    cell_text: BTreeMap<(String, String), String>,
    context_text: BTreeMap<String, String>,
    /// UI scratch: a row result's text box, by row id.
    result_text: BTreeMap<String, String>,
    /// UI scratch: the add-row box.
    new_row: String,
    /// UI scratch: the add-column boxes and the options bar's result-type and catch-all texts.
    new_column_id: String,
    new_column_kind: &'static str,
    new_column_binding: usize,
    result_type_text: String,
    catch_all_text: String,
    /// UI scratch: the add-column box's `FloatDistance` parameters.
    new_distance: DistanceForm,
    /// UI scratch: the catch-all's output value boxes (RON text), by output name.
    outputs_text: BTreeMap<String, String>,
}

/// The add-column box's `FloatDistance` parameters (the kind's defaults: influence 1, no wrap, no
/// drop; `max_distance` must be above 0 and starts at 1).
#[derive(Clone, Copy, Debug, PartialEq)]
struct DistanceForm {
    influence: f32,
    max_distance: f32,
    wrap: Option<(f32, f32)>,
    drop_beyond: bool,
}

impl Default for DistanceForm {
    fn default() -> Self {
        Self {
            influence: 1.0,
            max_distance: 1.0,
            wrap: None,
            drop_beyond: false,
        }
    }
}

impl DistanceForm {
    /// The column kind as RON text, spelled as `chooser.get` lists a `FloatDistance` column.
    fn kind_text(&self) -> String {
        let wrap = self
            .wrap
            .map_or_else(|| "None".to_owned(), |(lo, hi)| format!("Some(({lo:?}, {hi:?}))"));
        format!(
            "FloatDistance(influence: {:?}, max_distance: {:?}, wrap: {wrap}, drop_beyond: {})",
            self.influence, self.max_distance, self.drop_beyond
        )
    }
}

impl ChooserDoc {
    fn new(path: String, table: ChooserTable, nested: BTreeMap<String, ChooserTable>) -> Self {
        let result_type_text = ron::to_string(&table.result_type).unwrap_or_default();
        let catch_all_text = table
            .catch_all
            .as_ref()
            .map(|c| result_ron(&c.result))
            .unwrap_or_default();
        let mut doc = Self {
            path,
            table,
            nested,
            context: ChooserContext::default(),
            seed: 0,
            outcome: None,
            trace: ChooserTrace::default(),
            generation: 0,
            dirty: false,
            issues: Vec::new(),
            undo: Vec::new(),
            history_id: 0,
            cell_text: BTreeMap::new(),
            context_text: BTreeMap::new(),
            result_text: BTreeMap::new(),
            new_row: String::new(),
            new_column_id: String::new(),
            new_column_kind: "Bool",
            new_column_binding: 0,
            result_type_text,
            catch_all_text,
            new_distance: DistanceForm::default(),
            outputs_text: BTreeMap::new(),
        };
        doc.evaluate();
        doc
    }

    /// Re-runs validation and the traced evaluation (full detail, a fresh `Rng::new(seed)`, no recent picks).
    pub fn evaluate(&mut self) {
        let _span = tracing::trace_span!(target: "regia_editor", "tables_panel_evaluate").entered();
        self.issues = self.table.validate(&self.nested);
        let mut ctx = self.context.clone();
        let mut rng = Rng::new(self.seed);
        let (outcome, trace) =
            self.table
                .evaluate_traced(&self.nested, &mut ctx, &mut rng, None, TraceDetail::Full);
        self.outcome = outcome;
        self.trace = trace;
    }

    /// The state of cell (`row`, `column`) after the last evaluation.
    #[must_use]
    pub fn cell_state(&self, row: &str, column: &str) -> CellState {
        self.trace
            .rows
            .iter()
            .find(|r| r.id == row)
            .and_then(|r| r.cells.iter().find(|c| c.column == column))
            .map_or(CellState::Skipped, |c| match c.verdict {
                Verdict::Pass => CellState::Pass,
                Verdict::Fail => CellState::Fail,
                Verdict::Score(cost) => CellState::Score(cost),
                Verdict::Skipped => CellState::Skipped,
            })
    }

    /// The row's outcome code after the last evaluation: `selected`, `candidate`, `rejected`, `disabled`,
    /// `not_evaluated` or `nested_empty`.
    #[must_use]
    pub fn row_state(&self, row: &str) -> &'static str {
        match self.trace.rows.iter().find(|r| r.id == row).map(|r| &r.outcome) {
            Some(RowOutcome::Selected) => "selected",
            Some(RowOutcome::Candidate { .. }) => "candidate",
            Some(RowOutcome::RejectedBy(_)) => "rejected",
            Some(RowOutcome::Disabled) => "disabled",
            Some(RowOutcome::NestedEmpty) => "nested_empty",
            Some(RowOutcome::NotEvaluated) | None => "not_evaluated",
        }
    }

    fn detail(&self, row: &str, column: &str) -> String {
        self.trace
            .rows
            .iter()
            .find(|r| r.id == row)
            .and_then(|r| r.cells.iter().find(|c| c.column == column))
            .map(|c| c.detail.clone())
            .unwrap_or_default()
    }

    fn issue_at(&self, row: &str, column: &str) -> Option<&ChooserIssue> {
        self.issues
            .iter()
            .find(|i| i.row.as_deref() == Some(row) && i.column.as_deref() == Some(column))
    }

    /// Applies `edit` to a copy of the table; keeps it when its structure still validates.
    fn edit(
        &mut self,
        edit: impl FnOnce(&mut ChooserTable) -> Result<(), PanelError>,
    ) -> Result<(), PanelError> {
        let mut table = self.table.clone();
        edit(&mut table)?;
        let issues = table.validate_structure();
        if let Some(first) = issues.iter().find(|i| i.is_error()) {
            return Err(PanelError::new(first.code, first.to_string()));
        }
        if self.undo.len() == UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.undo.push(std::mem::replace(&mut self.table, table));
        self.generation += 1;
        self.dirty = true;
        self.cell_text.clear();
        self.result_text.clear();
        self.outputs_text.clear();
        self.result_type_text = ron::to_string(&self.table.result_type).unwrap_or_default();
        self.catch_all_text = self
            .table
            .catch_all
            .as_ref()
            .map(|c| result_ron(&c.result))
            .unwrap_or_default();
        self.evaluate();
        Ok(())
    }

    fn set_input_text(&mut self, name: &str, text: &str) -> Result<(), PanelError> {
        let kind = self
            .table
            .schema
            .inputs
            .get(name)
            .ok_or_else(|| err("unknown_input", format!("the table declares no input `{name}`")))?;
        if text.trim().is_empty() {
            self.context.unset(name);
        } else {
            let value = parse_value(kind, text).map_err(|e| err("bad_context", format!("`{name}`: {e}")))?;
            self.context.set(name, value);
        }
        Ok(())
    }

    fn set_cell_text(&mut self, row: &str, column: &str, text: &str) -> Result<(), PanelError> {
        self.edit(|table| {
            let col = table
                .column(column)
                .cloned()
                .ok_or_else(|| err("column_not_found", format!("no column `{column}`")))?;
            let cell = parse_cell(&col, &table.schema, text)
                .map_err(|e| err("bad_value", format!("`{text}`: {e}")))?;
            put_cell(table, row, column, cell)
        })
    }
}

/// Sets input `name` of `ctx` from JSON (`null` unsets it), checked against `table`'s schema.
fn apply_input(
    table: &ChooserTable,
    ctx: &mut ChooserContext,
    name: &str,
    value: &Value,
) -> Result<(), PanelError> {
    let kind = table
        .schema
        .inputs
        .get(name)
        .ok_or_else(|| err("unknown_input", format!("the table declares no input `{name}`")))?;
    if value.is_null() {
        ctx.unset(name);
    } else {
        let parsed = regia_agent::choosers::value_from_json(kind, value)
            .map_err(|e| err("bad_context", format!("`{name}`: {e}")))?;
        ctx.set(name, parsed);
    }
    Ok(())
}

fn put_cell(table: &mut ChooserTable, row: &str, column: &str, cell: Cell) -> Result<(), PanelError> {
    let target = table
        .rows
        .iter_mut()
        .find(|r| r.id == row)
        .ok_or_else(|| err("row_not_found", format!("no row `{row}`")))?;
    if cell == Cell::Any {
        target.cells.remove(column);
    } else {
        target.cells.insert(column.to_owned(), cell);
    }
    Ok(())
}

/// The Tables panel's state (an edit-world resource).
#[derive(Resource, Default)]
pub struct TablesPanel {
    /// The tab shown.
    pub tab: TablesTab,
    /// The open curve table.
    pub curves: Option<CurveDoc>,
    /// The open chooser.
    pub chooser: Option<ChooserDoc>,
    /// The open data table.
    pub data: Option<DataDoc>,
    /// The last failed operation.
    pub error: Option<PanelError>,
    /// The file the last save wrote, relative to `assets/`.
    pub last_saved: Option<String>,
    /// Import and Export: the file form (`exchange`).
    pub exchange_format: exchange::ExchangeFormat,
    /// Import and Export: the file (relative paths are under `assets/`).
    pub exchange_path: String,
    /// The last import or export.
    pub last_exchange: Option<exchange::ExchangeOutcome>,
    path_input: String,
    shared_depth: Option<usize>,
    history_requested: bool,
    retired: Vec<TableDocument>,
}

fn assets_root(project: Option<&Path>) -> Result<PathBuf, PanelError> {
    project
        .map(|p| p.join("assets"))
        .ok_or_else(|| err("no_project", "open a project first"))
}

fn read(root: &Path, path: &str, suffixes: &[&str]) -> Result<String, PanelError> {
    let file = confined(root, path, suffixes, PREFIX)?;
    std::fs::read_to_string(&file).map_err(|e| err("file_not_found", format!("{path}: {e}")))
}

/// Reads every table `table` reaches through `Chooser("...")` results, depth first, at most [`MAX_DEPTH`]
/// levels; unreadable ones are left out (validation reports them as unresolved).
fn load_nested(root: &Path, table: &ChooserTable, depth: u32, out: &mut BTreeMap<String, ChooserTable>) {
    if depth >= MAX_DEPTH {
        return;
    }
    let mut refs = BTreeSet::new();
    collect_refs(table, &mut refs);
    for path in refs {
        if out.contains_key(&path) {
            continue;
        }
        let Ok(text) = read(root, &path, CHOOSER_SUFFIXES) else {
            continue;
        };
        let Ok(nested) = ChooserTable::from_ron_str(&text) else {
            continue;
        };
        out.insert(path.clone(), nested.clone());
        load_nested(root, &nested, depth + 1, out);
    }
}

fn collect_refs(table: &ChooserTable, out: &mut BTreeSet<String>) {
    let results = table
        .rows
        .iter()
        .map(|r| &r.result)
        .chain(table.catch_all.iter().map(|c| &c.result));
    for result in results {
        match result {
            ResultEntry::Chooser(path) => {
                out.insert(path.clone());
            }
            ResultEntry::Inline(inner) => collect_refs(inner, out),
            _ => {}
        }
    }
}

impl TablesPanel {
    /// Opens `path` (under `assets/`): a `.curves.ron` / `.curves.csv` on the Curves tab, a `.chooser.ron`
    /// on the Chooser tab. An unsaved table on that tab is replaced.
    ///
    /// # Errors
    /// `tables_panel_no_project`, `tables_panel_bad_path`, `tables_panel_file_not_found`, or the parser's
    /// first error code (`curve_*`, `chooser_*`).
    pub fn open(&mut self, project: Option<&Path>, path: &str) -> Result<(), PanelError> {
        self.open_in(project, path, &SchemaLookup::none())
    }

    fn open_in(
        &mut self,
        project: Option<&Path>,
        path: &str,
        lookup: &SchemaLookup<'_>,
    ) -> Result<(), PanelError> {
        let root = assets_root(project)?;
        let lower = path.to_ascii_lowercase();
        if DATA_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
            let text = data::read_data(&root, path)?;
            let next = DataDoc::open(path, &text, lookup)?;
            if let Some(previous) = self.data.replace(next)
                && self.shared_depth.is_some()
            {
                self.retired.push(TableDocument::Data(Box::new(previous)));
            }
            self.tab = TablesTab::Data;
        } else if CHOOSER_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
            let text = read(&root, path, CHOOSER_SUFFIXES)?;
            let table =
                ChooserTable::from_ron_str(&text).map_err(|e| PanelError::new(e.code(), e.to_string()))?;
            let mut nested = BTreeMap::new();
            load_nested(&root, &table, 0, &mut nested);
            if let Some(previous) = self
                .chooser
                .replace(ChooserDoc::new(path.to_owned(), table, nested))
                && self.shared_depth.is_some()
            {
                self.retired.push(TableDocument::Chooser(Box::new(previous)));
            }
            self.tab = TablesTab::Chooser;
        } else {
            let text = read(&root, path, CURVE_SUFFIXES)?;
            let table = regia_framework::curve_table::parse_curve_table(path, &text)
                .map_err(|e| PanelError::new(e.code(), e.to_string()))?;
            let issues = table.validate();
            let selected = table.curves().first().map(|c| c.name.clone());
            let next = CurveDoc {
                path: path.to_owned(),
                table,
                selected,
                selected_key: None,
                generation: 0,
                dirty: false,
                issues,
                view: View2d::default(),
                csv: String::new(),
                undo: Vec::new(),
                history_id: 0,
                fitted: false,
                last_gesture: None,
                gestures: 0,
                dragging: None,
                name_input: String::new(),
            };
            if let Some(previous) = self.curves.replace(next)
                && self.shared_depth.is_some()
            {
                self.retired.push(TableDocument::Curves(Box::new(previous)));
            }
            self.tab = TablesTab::Curves;
        }
        self.error = None;
        Ok(())
    }

    fn curve_doc(&mut self) -> Result<&mut CurveDoc, PanelError> {
        self.curves
            .as_mut()
            .ok_or_else(|| err("no_table", "open a curve table first"))
    }

    fn chooser_doc(&mut self) -> Result<&mut ChooserDoc, PanelError> {
        self.chooser
            .as_mut()
            .ok_or_else(|| err("no_table", "open a chooser first"))
    }

    /// Applies one curve edit (`params.op`: `set_key`, `move_key`, `remove_key`, `set_curve`, `delete`,
    /// `select`, `import_csv`), the operations of `curves.*` on the open table.
    ///
    /// # Errors
    /// `tables_panel_no_table`, `tables_panel_bad_op`, `tables_panel_bad_value`, or the refused edit's
    /// validator code (`curve_*`).
    pub fn curve_edit(&mut self, params: &Value) -> Result<(), PanelError> {
        let doc = self.curve_doc()?;
        let op = params.get("op").and_then(Value::as_str).unwrap_or_default();
        let name = params.get("name").and_then(Value::as_str);
        let need_name = || name.ok_or_else(|| err("bad_value", "`name` must be a curve name"));
        let number = |key: &str| -> Result<Option<f32>, PanelError> {
            opt_f32(params, key).map_err(|e| err("bad_value", e.message))
        };
        let required = |key: &str| -> Result<f32, PanelError> {
            number(key)?.ok_or_else(|| err("bad_value", format!("`{key}` must be a finite number")))
        };
        let interp = match params.get("interp").and_then(Value::as_str) {
            None => None,
            Some(text) => Some(
                TableInterp::from_name(text)
                    .ok_or_else(|| err("bad_value", format!("unknown interpolation `{text}`")))?,
            ),
        };
        let mut table = doc.table.clone();
        let result = match op {
            "select" => {
                let name = need_name()?;
                if table.curve(name).is_none() {
                    return Err(err("curve_not_found", format!("no curve `{name}`")));
                }
                doc.selected = Some(name.to_owned());
                doc.selected_key = number("x")?.and_then(|x| {
                    table
                        .curve(name)
                        .and_then(|c| c.keys.iter().position(|k| k.x == x))
                });
                return Ok(());
            }
            "set_key" => {
                let mut key = CurveKey::new(required("x")?, required("y")?);
                key.interp = interp;
                key.in_tangent = number("in_tangent")?;
                key.out_tangent = number("out_tangent")?;
                table.set_key(need_name()?, key)
            }
            "move_key" => {
                let name = need_name()?;
                let from = required("from_x")?;
                let (x, y) = (required("x")?, required("y")?);
                table.edit(|curves| {
                    let curve = curves.iter_mut().find(|c| c.name == name).ok_or_else(|| {
                        CurveIssue::error("curve_not_found", format!("no curve `{name}`")).with_curve(name)
                    })?;
                    let at = curve.keys.iter().position(|k| k.x == from).ok_or_else(|| {
                        CurveIssue::error("key_not_found", format!("no key at x = {from}")).with_curve(name)
                    })?;
                    let mut key = curve.keys.remove(at);
                    if curve.keys.iter().any(|k| k.x == x) {
                        return Err(CurveIssue::error(
                            "curve_duplicate_x",
                            format!("a key already sits at x = {x}"),
                        )
                        .with_curve(name));
                    }
                    key.x = x;
                    key.y = y;
                    let to = curve.keys.partition_point(|k| k.x < x);
                    curve.keys.insert(to, key);
                    Ok(())
                })
            }
            "remove_key" => table.remove_key(need_name()?, required("x")?),
            "set_tangent" => {
                let name = need_name()?;
                let x = required("x")?;
                let side = params.get("side").and_then(Value::as_str).unwrap_or("both");
                if !matches!(side, "in" | "out" | "both") {
                    return Err(err(
                        "bad_value",
                        format!("`side` must be in, out or both, not `{side}`"),
                    ));
                }
                // `slope: null` returns the side to the derived tangent.
                let slope = match params.get("slope") {
                    None => return Err(err("bad_value", "`slope` must be a finite number or null")),
                    Some(Value::Null) => None,
                    Some(_) => Some(required("slope")?),
                };
                table.edit(|curves| {
                    let curve = curves.iter_mut().find(|c| c.name == name).ok_or_else(|| {
                        CurveIssue::error("curve_not_found", format!("no curve `{name}`")).with_curve(name)
                    })?;
                    let key = curve.keys.iter_mut().find(|k| k.x == x).ok_or_else(|| {
                        CurveIssue::error("key_not_found", format!("no key at x = {x}")).with_curve(name)
                    })?;
                    if side != "out" {
                        key.in_tangent = slope;
                    }
                    if side != "in" {
                        key.out_tangent = slope;
                    }
                    Ok(())
                })
            }
            "set_curve" => {
                let name = need_name()?;
                let mut curve = table
                    .curve(name)
                    .cloned()
                    .unwrap_or_else(|| NamedCurve::new(name, vec![CurveKey::new(0.0, 0.0)]));
                for (key, slot) in [("pre", &mut curve.pre), ("post", &mut curve.post)] {
                    if let Some(text) = params.get(key).and_then(Value::as_str) {
                        *slot = Extrap::from_name(text)
                            .ok_or_else(|| err("bad_value", format!("unknown extrapolation `{text}`")))?;
                    }
                }
                if params.get("interp").is_some() {
                    curve.interp = interp;
                }
                let rename = params.get("rename").and_then(Value::as_str);
                table.upsert_curve(curve).and_then(|()| match rename {
                    Some(to) if to != name => table.rename_curve(name, to),
                    _ => Ok(()),
                })
            }
            "delete" => table.delete_curve(need_name()?),
            "import_csv" => {
                let csv = params
                    .get("csv")
                    .and_then(Value::as_str)
                    .ok_or_else(|| err("bad_value", "`csv` must be CSV text"))?;
                CurveTable::from_csv_str(csv, interp.unwrap_or(TableInterp::Linear)).map(|t| table = t)
            }
            other => return Err(err("bad_op", format!("unknown curve edit `{other}`"))),
        };
        result.map_err(|e| PanelError::new(e.code(), e.to_string()))?;
        // The steps of one drag (`gesture`) are one undo step.
        let gesture = params.get("gesture").and_then(Value::as_u64);
        if gesture.is_some() && gesture == doc.last_gesture && !doc.undo.is_empty() {
            doc.table = table;
        } else {
            if doc.undo.len() == UNDO_DEPTH {
                doc.undo.remove(0);
            }
            doc.undo.push(std::mem::replace(&mut doc.table, table));
        }
        doc.last_gesture = gesture;
        doc.generation += 1;
        doc.dirty = true;
        doc.issues = doc.table.validate();
        // The selection follows a rename and survives everything else that keeps the curve.
        if op == "set_curve"
            && let Some(to) = params.get("rename").and_then(Value::as_str)
        {
            doc.selected = Some(to.to_owned());
        }
        if doc
            .selected
            .as_deref()
            .is_none_or(|s| doc.table.curve(s).is_none())
        {
            doc.selected = doc.table.curves().first().map(|c| c.name.clone());
            doc.selected_key = None;
        }
        if op == "move_key"
            && let (Some(curve), Some(x)) = (
                doc.selected.as_deref().and_then(|s| doc.table.curve(s)),
                number("x")?,
            )
        {
            doc.selected_key = curve.keys.iter().position(|k| k.x == x);
        }
        Ok(())
    }

    /// Applies one chooser edit (`params.op`: `set_cell`, `set_row`, `move_row`, `remove_row`,
    /// `set_column`, `remove_column`, `set_options`), the operations of `chooser.*` on the open
    /// table, then evaluates the test context again. The `set_row`, `remove_row`, `set_column`
    /// (object form), `remove_column` and `set_options` arms call the very functions the agent
    /// methods call (`regia_agent::choosers::{set_row, remove_row, set_column, remove_column,
    /// set_options}`), so a change through the panel writes the same file bytes the agent method
    /// writes, as one undo step.
    ///
    /// # Errors
    /// `tables_panel_no_table`, `tables_panel_bad_op`, `tables_panel_row_not_found`,
    /// `tables_panel_column_not_found`, `bad_value`, or the refused edit's validator code
    /// (`chooser_*`).
    pub fn chooser_edit(&mut self, params: &Value) -> Result<(), PanelError> {
        let doc = self.chooser_doc()?;
        let op = params.get("op").and_then(Value::as_str).unwrap_or_default();
        let field = |key: &str| -> Result<String, PanelError> {
            str_param(params, key)
                .map(str::to_owned)
                .map_err(|e| err("bad_value", e.message))
        };
        // The shared agent edits carry their own reasons (row_not_found, bad_value, ...); the
        // panel passes them on as refused edits, like the validator's chooser_* codes.
        let shared = |e: RpcError| from_rpc(&e, "bad_value");
        match op {
            "set_cell" => {
                let (row, column) = (field("row")?, field("column")?);
                if let Some(text) = params.get("text").and_then(Value::as_str) {
                    return doc.set_cell_text(&row, &column, text);
                }
                let value = params.get("value").cloned().unwrap_or(Value::Null);
                doc.edit(|table| {
                    let col = table
                        .column(&column)
                        .cloned()
                        .ok_or_else(|| err("column_not_found", format!("no column `{column}`")))?;
                    let cell = regia_agent::choosers::cell_from_json(table, &col, &value)
                        .map_err(|e| crate::data_panels::from_rpc(&e, "bad_value"))?;
                    put_cell(table, &row, &column, cell)
                })
            }
            "set_row" => doc.edit(|table| regia_agent::choosers::set_row(table, params).map_err(shared)),
            "remove_row" => {
                let row = field("row")?;
                doc.edit(|table| regia_agent::choosers::remove_row(table, &row).map_err(shared))
            }
            "move_row" => {
                let row = field("row")?;
                let before = params.get("before").and_then(Value::as_str).map(str::to_owned);
                doc.edit(|table| {
                    let at = table
                        .rows
                        .iter()
                        .position(|r| r.id == row)
                        .ok_or_else(|| err("row_not_found", format!("no row `{row}`")))?;
                    let moved = table.rows.remove(at);
                    let to = match &before {
                        None => table.rows.len(),
                        Some(b) => table
                            .rows
                            .iter()
                            .position(|r| &r.id == b)
                            .ok_or_else(|| err("row_not_found", format!("no row `{b}`")))?,
                    };
                    table.rows.insert(to, moved);
                    Ok(())
                })
            }
            "set_column" => {
                if params.get("column").is_some_and(Value::is_object) {
                    return doc
                        .edit(|table| regia_agent::choosers::set_column(table, params).map_err(shared));
                }
                let column = field("column")?;
                let disabled = opt_bool(params, "disabled").map_err(|e| err("bad_value", e.message))?;
                doc.edit(|table| {
                    let target = table
                        .columns
                        .iter_mut()
                        .find(|c| c.id == column)
                        .ok_or_else(|| err("column_not_found", format!("no column `{column}`")))?;
                    if let Some(disabled) = disabled {
                        target.disabled = disabled;
                    }
                    Ok(())
                })
            }
            "remove_column" => {
                let column = field("column")?;
                doc.edit(|table| regia_agent::choosers::remove_column(table, &column).map_err(shared))
            }
            "set_options" => {
                doc.edit(|table| regia_agent::choosers::set_options(table, params).map_err(shared))
            }
            other => Err(err("bad_op", format!("unknown chooser edit `{other}`"))),
        }
    }

    /// Sets the chooser's test context (`context: { input: value | null }`; `replace: true` clears the
    /// others first) and seed, then evaluates again.
    ///
    /// # Errors
    /// `tables_panel_no_table`, `tables_panel_unknown_input`, `tables_panel_bad_context`; nothing changes.
    pub fn set_context(&mut self, params: &Value) -> Result<(), PanelError> {
        let doc = self.chooser_doc()?;
        let mut context = if params.get("replace").and_then(Value::as_bool).unwrap_or(false) {
            ChooserContext::default()
        } else {
            doc.context.clone()
        };
        match params.get("context") {
            None | Some(Value::Null) => {}
            Some(Value::Object(map)) => {
                for (name, value) in map {
                    apply_input(&doc.table, &mut context, name, value)?;
                }
            }
            Some(_) => return Err(err("bad_context", "`context` must be an object")),
        }
        let seed = match params.get("seed") {
            None | Some(Value::Null) => doc.seed,
            Some(v) => v
                .as_u64()
                .ok_or_else(|| err("bad_context", "`seed` must be an unsigned integer"))?,
        };
        doc.context = context;
        doc.seed = seed;
        doc.context_text.clear();
        doc.evaluate();
        Ok(())
    }

    /// Undoes the last edit of the table on `tab` (the shown tab by default).
    ///
    /// # Errors
    /// `tables_panel_no_table`, `tables_panel_nothing_to_undo`.
    pub fn undo(&mut self, tab: Option<TablesTab>) -> Result<(), PanelError> {
        self.undo_in(tab, &SchemaLookup::none())
    }

    fn undo_in(&mut self, tab: Option<TablesTab>, lookup: &SchemaLookup<'_>) -> Result<(), PanelError> {
        if let Some(depth) = self.shared_depth {
            if depth == 0 {
                return Err(err("nothing_to_undo", "nothing to undo"));
            }
            self.history_requested = true;
            return Ok(());
        }
        match tab.unwrap_or(self.tab) {
            TablesTab::Data => {
                self.data
                    .as_mut()
                    .ok_or_else(|| err("no_table", "open a data table first"))?
                    .undo(lookup)?;
            }
            TablesTab::Curves => {
                let doc = self.curve_doc()?;
                let previous = doc
                    .undo
                    .pop()
                    .ok_or_else(|| err("nothing_to_undo", "nothing to undo"))?;
                doc.table = previous;
                doc.generation += 1;
                doc.dirty = true;
                doc.issues = doc.table.validate();
                if doc
                    .selected
                    .as_deref()
                    .is_none_or(|s| doc.table.curve(s).is_none())
                {
                    doc.selected = doc.table.curves().first().map(|c| c.name.clone());
                }
                doc.selected_key = None;
            }
            TablesTab::Chooser => {
                let doc = self.chooser_doc()?;
                let previous = doc
                    .undo
                    .pop()
                    .ok_or_else(|| err("nothing_to_undo", "nothing to undo"))?;
                doc.table = previous;
                doc.generation += 1;
                doc.dirty = true;
                doc.cell_text.clear();
                doc.result_text.clear();
                doc.outputs_text.clear();
                doc.result_type_text = ron::to_string(&doc.table.result_type).unwrap_or_default();
                doc.catch_all_text = doc
                    .table
                    .catch_all
                    .as_ref()
                    .map(|c| result_ron(&c.result))
                    .unwrap_or_default();
                doc.evaluate();
            }
        }
        Ok(())
    }

    /// Writes the table on `tab` (the shown tab by default) to its file, atomically: canonical RON, or CSV
    /// for a `.curves.csv`. Returns the path written (under `assets/`).
    ///
    /// # Errors
    /// `tables_panel_no_project`, `tables_panel_no_table`, `tables_panel_io_failed`, or a CSV writer code.
    pub fn save(&mut self, project: Option<&Path>, tab: Option<TablesTab>) -> Result<String, PanelError> {
        let root = assets_root(project)?;
        let (path, text) = match tab.unwrap_or(self.tab) {
            TablesTab::Curves => {
                let doc = self.curve_doc()?;
                let text = if doc.path.to_ascii_lowercase().ends_with(".csv") {
                    doc.table
                        .to_csv_string()
                        .map_err(|e| PanelError::new(e.code(), e.to_string()))?
                } else {
                    doc.table.to_ron_string()
                };
                (doc.path.clone(), text)
            }
            TablesTab::Chooser => {
                let doc = self.chooser_doc()?;
                (doc.path.clone(), doc.table.to_ron_string())
            }
            TablesTab::Data => {
                let doc = self
                    .data
                    .as_ref()
                    .ok_or_else(|| err("no_table", "open a data table first"))?;
                (doc.path.clone(), doc.text())
            }
        };
        let suffixes: Vec<&str> = CURVE_SUFFIXES
            .iter()
            .chain(CHOOSER_SUFFIXES)
            .chain(DATA_SUFFIXES)
            .copied()
            .collect();
        let file = confined(&root, &path, &suffixes, PREFIX)?;
        write_atomic(&file, &text, PREFIX)?;
        match tab.unwrap_or(self.tab) {
            TablesTab::Curves => {
                if let Some(doc) = self.curves.as_mut() {
                    doc.dirty = false;
                }
            }
            TablesTab::Chooser => {
                if let Some(doc) = self.chooser.as_mut() {
                    doc.dirty = false;
                }
            }
            TablesTab::Data => {
                if let Some(doc) = self.data.as_mut() {
                    doc.dirty = false;
                }
            }
        }
        self.last_saved = Some(path.clone());
        Ok(path)
    }

    /// The panel as the agent reads it.
    #[must_use]
    pub fn json(&self) -> Value {
        json!({
            "tab": self.tab.name(),
            "curves": self.curves.as_ref().map(curves_json),
            "chooser": self.chooser.as_ref().map(chooser_json),
            "data": self.data.as_ref().map(DataDoc::json),
            "error": self.error.as_ref().map(PanelError::json),
            "last_saved": self.last_saved,
        })
    }
}

fn curves_json(doc: &CurveDoc) -> Value {
    json!({
        "path": doc.path,
        "interp": doc.table.interp().name(),
        "generation": doc.generation,
        "dirty": doc.dirty,
        "undo_depth": doc.undo.len(),
        "selected": doc.selected,
        "selected_key": doc.selected_key,
        "curves": doc.table.curves().iter().map(|c| json!({
            "name": c.name,
            "pre": c.pre.name(),
            "post": c.post.name(),
            "interp": c.interp.map(TableInterp::name),
            "keys": c.keys.iter().map(|k| json!({
                "x": k.x, "y": k.y, "interp": k.interp.map(TableInterp::name),
                "in_tangent": k.in_tangent, "out_tangent": k.out_tangent,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "issues": regia_agent::curve_tables::issues_json(&doc.issues),
    })
}

fn chooser_json(doc: &ChooserDoc) -> Value {
    let table = &doc.table;
    json!({
        "path": doc.path,
        "mode": table.mode.name(),
        "generation": doc.generation,
        "dirty": doc.dirty,
        "undo_depth": doc.undo.len(),
        "nested": doc.nested.keys().collect::<Vec<_>>(),
        "inputs": table.schema.inputs.iter().map(|(name, kind)| json!({ "name": name, "kind": kind.name() })).collect::<Vec<_>>(),
        "context": doc.context.inputs().map(|(k, v)| (k.to_owned(), regia_agent::choosers::value_json(v))).collect::<serde_json::Map<_, _>>(),
        "seed": doc.seed,
        "columns": table.columns.iter().map(|c| json!({
            "id": c.id, "kind": c.kind.ron_name(), "input": c.input, "output": c.output, "disabled": c.disabled,
        })).collect::<Vec<_>>(),
        "rows": table.rows.iter().map(|r| json!({
            "id": r.id,
            "enabled": r.enabled,
            "state": doc.row_state(&r.id),
            "result": result_ron(&r.result),
            "cells": table.columns.iter().map(|c| json!({
                "column": c.id,
                "cell": regia_agent::choosers::cell_json(r.cell(&c.id)),
                "state": doc.cell_state(&r.id, &c.id).name(),
                "cost": match doc.cell_state(&r.id, &c.id) { CellState::Score(cost) => Some(cost), _ => None },
                "detail": doc.detail(&r.id, &c.id),
                "issue": doc.issue_at(&r.id, &c.id).map(|i| i.code),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "outcome": {
            "row": doc.outcome.as_ref().and_then(|o| o.row.clone()),
            "used_catch_all": doc.outcome.as_ref().is_some_and(|o| o.used_catch_all),
            "result": doc.outcome.as_ref().map(|o| result_ron(&o.result)),
            "outputs": doc.outcome.as_ref().map(|o| o.outputs.iter().map(|(k, v)| (k.clone(), regia_agent::choosers::value_json(v))).collect::<serde_json::Map<_, _>>()),
            "draw": doc.trace.draw,
        },
        "issues": regia_agent::choosers::issues_json(&doc.issues),
    })
}

// ------------------------------------------------------------------------------------------- drawing

/// Draws the panel.
pub fn tables_panel(ui: &mut egui::Ui, world: &mut World) {
    let project = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<TablesPanel>().unwrap_or_default();
    prepare_shared(world, &mut panel);
    draw(ui, &mut panel, project.as_deref(), &SchemaLookup::of(world));
    finish_shared(world, panel, Some(ui.ctx().cumulative_frame_nr()));
}

fn draw(ui: &mut egui::Ui, panel: &mut TablesPanel, project: Option<&Path>, lookup: &SchemaLookup<'_>) {
    let _span = tracing::trace_span!(target: "regia_editor", "tables_panel_draw").entered();
    crate::data_panels::header(ui, "Tables", "curve tables · choosers · data tables");
    ui.horizontal(|ui| {
        ui.selectable_value(&mut panel.tab, TablesTab::Curves, "Curves");
        ui.selectable_value(&mut panel.tab, TablesTab::Chooser, "Chooser");
        ui.selectable_value(&mut panel.tab, TablesTab::Data, "Data");
        ui.separator();
        ui.label("Open");
        ui.add(egui::TextEdit::singleline(&mut panel.path_input).hint_text("tables/falloff.curves.ron"));
        if ui.button("Open").clicked() {
            let path = panel.path_input.trim().to_owned();
            if let Err(e) = panel.open_in(project, &path, lookup) {
                panel.error = Some(e);
            }
        }
    });
    let mut result = Ok(());
    ui.horizontal(|ui| {
        let (path, dirty, depth) = match panel.tab {
            TablesTab::Curves => panel
                .curves
                .as_ref()
                .map(|d| (d.path.clone(), d.dirty, d.undo.len())),
            TablesTab::Chooser => panel
                .chooser
                .as_ref()
                .map(|d| (d.path.clone(), d.dirty, d.undo.len())),
            TablesTab::Data => panel
                .data
                .as_ref()
                .map(|d| (d.path.clone(), d.dirty, d.undo_depth())),
        }
        .unwrap_or_default();
        if path.is_empty() {
            ui.weak("nothing open");
            return;
        }
        ui.monospace(if dirty { format!("{path} *") } else { path });
        let depth = panel.shared_depth.unwrap_or(depth);
        if ui.add_enabled(depth > 0, egui::Button::new("Undo")).clicked() {
            result = panel.undo_in(None, lookup);
        }
        if ui.add_enabled(dirty, egui::Button::new("Save")).clicked() {
            result = panel.save(project, None).map(|_| ());
        }
    });
    if let Err(e) = result {
        panel.error = Some(e);
    }
    if let Some(Err(e)) = exchange::draw_exchange(ui, panel, project, lookup) {
        panel.error = Some(e);
    }
    if let Some(e) = &panel.error {
        e.draw(ui);
    }
    ui.separator();
    let outcome = match panel.tab {
        TablesTab::Curves => draw_curves(ui, panel),
        TablesTab::Chooser => draw_chooser(ui, panel),
        TablesTab::Data => match panel.data.as_mut() {
            Some(doc) => data::draw_data(ui, doc, lookup),
            None => {
                ui.weak("Open a .table.ron or .table.csv under assets/.");
                Ok(())
            }
        },
    };
    if let Err(e) = outcome {
        panel.error = Some(e);
    }
}

fn curve_color(index: usize) -> egui::Color32 {
    CURVE_COLORS[index % CURVE_COLORS.len()]
}

fn curve_bounds(table: &CurveTable) -> [f32; 4] {
    let mut b = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
    for k in table.curves().iter().flat_map(|c| &c.keys) {
        b = [b[0].min(k.x), b[1].min(k.y), b[2].max(k.x), b[3].max(k.y)];
    }
    if b[0].is_finite() { b } else { [0.0, 0.0, 1.0, 1.0] }
}

fn draw_curves(ui: &mut egui::Ui, panel: &mut TablesPanel) -> Result<(), PanelError> {
    let Some(doc) = panel.curves.as_mut() else {
        ui.weak("Open a .curves.ron or .curves.csv under assets/.");
        return Ok(());
    };
    let mut edits: Vec<Value> = Vec::new();
    ui.horizontal(|ui| {
        for (i, curve) in doc.table.curves().iter().enumerate() {
            let selected = doc.selected.as_deref() == Some(curve.name.as_str());
            let text = egui::RichText::new(&curve.name).color(curve_color(i));
            if ui.selectable_label(selected, text).clicked() {
                edits.push(json!({ "op": "select", "name": curve.name }));
            }
        }
    });
    // The add, rename and delete bar: the same set_curve / delete ops the agent's curves.*
    // methods run (one undo step each; a rename to an existing name is refused by the library).
    ui.horizontal(|ui| {
        let name = doc.name_input.trim().to_owned();
        ui.add(
            egui::TextEdit::singleline(&mut doc.name_input)
                .hint_text("curve name")
                .desired_width(110.0),
        );
        let fresh = !name.is_empty() && doc.table.curve(&name).is_none();
        if ui
            .add_enabled(fresh, egui::Button::new("Add curve"))
            .on_hover_text("Create the curve with one key at (0, 0)")
            .clicked()
        {
            edits.push(json!({ "op": "set_curve", "name": name.as_str() }));
            edits.push(json!({ "op": "select", "name": name.as_str() }));
            doc.name_input.clear();
        }
        if let Some(selected) = doc.selected.clone() {
            let rename = fresh && name != selected;
            if ui
                .add_enabled(rename, egui::Button::new("Rename"))
                .on_hover_text("Rename the selected curve")
                .clicked()
            {
                edits.push(json!({ "op": "set_curve", "name": selected, "rename": name.as_str() }));
            }
            if ui.button("Delete curve").clicked() {
                edits.push(json!({ "op": "delete", "name": selected }));
            }
        }
    });
    let selected = doc.selected.clone();
    if let Some(name) = &selected
        && let Some(curve) = doc.table.curve(name).cloned()
    {
        ui.horizontal(|ui| {
            let interp = curve.interp.map_or("table", TableInterp::name);
            egui::ComboBox::from_label("interp")
                .selected_text(interp)
                .show_ui(ui, |ui| {
                    for choice in [TableInterp::Step, TableInterp::Linear, TableInterp::Cubic] {
                        if ui
                            .selectable_label(curve.interp == Some(choice), choice.name())
                            .clicked()
                        {
                            edits.push(json!({ "op": "set_curve", "name": name, "interp": choice.name() }));
                        }
                    }
                });
            for (side, current) in [("pre", curve.pre), ("post", curve.post)] {
                egui::ComboBox::from_label(side)
                    .selected_text(current.name())
                    .show_ui(ui, |ui| {
                        for choice in [
                            Extrap::Hold,
                            Extrap::Linear,
                            Extrap::Cycle,
                            Extrap::CycleOffset,
                            Extrap::PingPong,
                        ] {
                            if ui.selectable_label(current == choice, choice.name()).clicked() {
                                edits.push(json!({ "op": "set_curve", "name": name, side: choice.name() }));
                            }
                        }
                    });
            }
        });
    }
    // The graph.
    let size = egui::vec2(ui.available_width().max(160.0), 220.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    doc.view.rect = rect;
    if !doc.fitted {
        doc.view.zoom_to_fit(curve_bounds(&doc.table), 0.1);
        doc.fitted = true;
    }
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(22));
    doc.view.paint_grid(&painter, |x| format!("{x:.2}"), true);
    let steps = (rect.width() / 2.0).max(8.0) as usize;
    for (i, curve) in doc.table.curves().iter().enumerate() {
        let is_selected = selected.as_deref() == Some(curve.name.as_str());
        let line: Vec<egui::Pos2> = (0..=steps)
            .filter_map(|s| {
                let sx = rect.left() + rect.width() * s as f32 / steps as f32;
                let x = doc.view.to_value(egui::pos2(sx, 0.0)).x;
                doc.table
                    .sample_named(&curve.name, x)
                    .filter(|y| y.is_finite())
                    .map(|y| doc.view.to_screen(egui::vec2(x, y)))
            })
            .collect();
        let width = if is_selected { 2.5 } else { 1.0 };
        painter.add(egui::Shape::line(line, egui::Stroke::new(width, curve_color(i))));
        if is_selected {
            for (k, key) in curve.keys.iter().enumerate() {
                let fill = if doc.selected_key == Some(k) {
                    egui::Color32::from_rgb(255, 210, 60)
                } else {
                    egui::Color32::WHITE
                };
                let at = doc.view.to_screen(egui::vec2(key.x, key.y));
                if doc.selected_key == Some(k) {
                    for (out, side) in [(false, -1.0), (true, 1.0)] {
                        let slope = display_slope(&doc.table, curve, k, out);
                        let h = crate::anim::curve_view::handle_position(&doc.view, at, slope, side);
                        painter.line_segment([at, h], egui::Stroke::new(1.0, egui::Color32::from_gray(160)));
                        painter.circle_filled(h, 3.5, egui::Color32::from_rgb(230, 200, 90));
                    }
                }
                painter.rect_filled(egui::Rect::from_center_size(at, egui::vec2(7.0, 7.0)), 1.0, fill);
            }
        }
    }
    if let Some(name) = selected.as_deref()
        && let Some(curve) = doc.table.curve(name).cloned()
    {
        curve_gestures(ui, doc, &response, name, &curve, &mut edits);
        // The selected key.
        if let Some(key) = doc.selected_key.and_then(|k| curve.keys.get(k)) {
            ui.horizontal(|ui| {
                ui.label(format!("{name} key"));
                let (mut x, mut y) = (key.x, key.y);
                let dx = ui.add(egui::DragValue::new(&mut x).speed(0.01).prefix("x "));
                let dy = ui.add(egui::DragValue::new(&mut y).speed(0.01).prefix("y "));
                if (dx.changed() || dy.changed()) && x.is_finite() && y.is_finite() {
                    edits.push(json!({ "op": "move_key", "name": name, "from_x": key.x, "x": x, "y": y }));
                }
                if ui.button("Auto tangents").clicked() {
                    edits.push(json!({ "op": "set_tangent", "name": name, "x": key.x, "slope": null }));
                }
                if ui.button("Delete key").clicked() {
                    edits.push(json!({ "op": "remove_key", "name": name, "x": key.x }));
                }
            });
        }
    }
    // The grid: the CSV layout, one x per column and one row per curve.
    egui::CollapsingHeader::new("Grid").default_open(true).show(ui, |ui| {
        let axis = grid_axis(&doc.table);
        egui::ScrollArea::horizontal().id_salt("tables-curve-grid").show(ui, |ui| {
            egui::Grid::new("tables-curve-grid-cells").striped(true).show(ui, |ui| {
                ui.strong("name");
                for x in &axis {
                    ui.strong(format!("{x}"));
                }
                ui.end_row();
                for curve in doc.table.curves() {
                    ui.label(&curve.name);
                    for &x in &axis {
                        match curve.keys.iter().find(|k| k.x == x) {
                            Some(key) => {
                                let mut y = key.y;
                                if ui.add(egui::DragValue::new(&mut y).speed(0.01)).changed() && y.is_finite() {
                                    edits.push(json!({ "op": "move_key", "name": curve.name, "from_x": x, "x": x, "y": y }));
                                }
                            }
                            None => {
                                if ui.small_button("+").on_hover_text("Add a key here").clicked() {
                                    let y = doc.table.sample_named(&curve.name, x).unwrap_or(0.0);
                                    edits.push(json!({ "op": "set_key", "name": curve.name, "x": x, "y": y }));
                                }
                            }
                        }
                    }
                    ui.end_row();
                }
            });
        });
        if axis.len() == GRID_COLUMNS {
            ui.weak(format!("the first {GRID_COLUMNS} positions"));
        }
    });
    ui.collapsing("Paste CSV", |ui| {
        ui.add(
            egui::TextEdit::multiline(&mut doc.csv)
                .desired_rows(3)
                .code_editor(),
        );
        if ui.button("Replace the table with this CSV").clicked() {
            edits.push(json!({ "op": "import_csv", "csv": doc.csv }));
        }
    });
    for issue in &doc.issues {
        ui.colored_label(egui::Color32::from_rgb(230, 170, 80), issue.to_string());
    }
    for edit in edits {
        panel.curve_edit(&edit)?;
    }
    Ok(())
}

fn curve_gestures(
    ui: &egui::Ui,
    doc: &mut CurveDoc,
    response: &egui::Response,
    name: &str,
    curve: &NamedCurve,
    edits: &mut Vec<Value>,
) {
    let screen: Vec<egui::Pos2> = curve
        .keys
        .iter()
        .map(|k| doc.view.to_screen(egui::vec2(k.x, k.y)))
        .collect();
    let pick = |p: egui::Pos2| screen.iter().position(|s| s.distance(p) < 6.0);
    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0
            && let Some(anchor) = response.hover_pos()
        {
            let k = (1.0 + scroll * 0.002).clamp(0.5, 2.0);
            doc.view.zoom_about(anchor, egui::vec2(k, k));
        }
    }
    if response.dragged_by(egui::PointerButton::Secondary) || response.dragged_by(egui::PointerButton::Middle)
    {
        doc.view.pan(response.drag_delta());
        return;
    }
    // The selected key's tangent handles, hit before the keys.
    let handle_at = |k: usize, out: bool| {
        let slope = display_slope(&doc.table, curve, k, out);
        crate::anim::curve_view::handle_position(&doc.view, screen[k], slope, if out { 1.0 } else { -1.0 })
    };
    if response.drag_started_by(egui::PointerButton::Primary) {
        let origin = ui
            .input(|i| i.pointer.press_origin())
            .or(response.interact_pointer_pos());
        let handle = doc.selected_key.filter(|&k| k < screen.len()).and_then(|k| {
            let p = origin?;
            [false, true]
                .into_iter()
                .find(|&out| handle_at(k, out).distance(p) < 6.0)
                .map(|out| CurveDrag::Handle { key: k, out })
        });
        doc.dragging = handle.or_else(|| origin.and_then(pick).map(CurveDrag::Key));
        if let Some(CurveDrag::Key(k)) = doc.dragging {
            doc.selected_key = Some(k);
        }
        doc.gestures += 1;
    }
    if response.drag_stopped() {
        doc.dragging = None;
    }
    let gesture = doc.gestures;
    if response.dragged_by(egui::PointerButton::Primary)
        && let Some(p) = response.interact_pointer_pos()
    {
        match doc.dragging {
            Some(CurveDrag::Key(k)) => {
                if let Some(key) = curve.keys.get(k) {
                    let v = doc.view.to_value(p);
                    if v.x.is_finite() && v.y.is_finite() && (v.x != key.x || v.y != key.y) {
                        edits.push(json!({ "op": "move_key", "name": name, "from_x": key.x, "x": v.x, "y": v.y, "gesture": gesture }));
                    }
                }
            }
            Some(CurveDrag::Handle { key, out }) => {
                if let Some(k) = curve.keys.get(key) {
                    let slope = crate::anim::curve_view::slope_from_handle(&doc.view, screen[key], p);
                    // Alt breaks the tangent: only the dragged side moves.
                    let broken = ui.input(|i| i.modifiers.alt);
                    let side = match (broken, out) {
                        (false, _) => "both",
                        (true, true) => "out",
                        (true, false) => "in",
                    };
                    if slope.is_finite() {
                        edits.push(json!({ "op": "set_tangent", "name": name, "x": k.x, "side": side, "slope": slope, "gesture": gesture }));
                    }
                }
            }
            None => {}
        }
    }
    if response.double_clicked()
        && let Some(p) = response.interact_pointer_pos()
        && pick(p).is_none()
    {
        let x = doc.view.to_value(p).x;
        let y = doc.table.sample_named(name, x).unwrap_or(0.0);
        edits.push(json!({ "op": "set_key", "name": name, "x": x, "y": y }));
    } else if response.clicked()
        && let Some(p) = response.interact_pointer_pos()
    {
        doc.selected_key = pick(p);
    }
}

/// The column kinds the add-column box offers (RON names; `FloatDistance` shows its parameters,
/// `DISTANCE_KIND`).
const ADDABLE_COLUMN_KINDS: &[&str] = &[
    "Bool",
    "IntRange",
    "FloatRange",
    DISTANCE_KIND,
    "Enum",
    "Name",
    "Tags",
    "Asset",
    "Weight",
    "SetBool",
    "SetInt",
    "SetFloat",
    "SetEnum",
    "SetName",
    "SetTags",
    "SetAsset",
];

/// The add-column kind that takes parameters (influence, max distance, wrap, drop beyond).
const DISTANCE_KIND: &str = "FloatDistance";

fn draw_chooser(ui: &mut egui::Ui, panel: &mut TablesPanel) -> Result<(), PanelError> {
    let Some(doc) = panel.chooser.as_mut() else {
        ui.weak("Open a .chooser.ron under assets/.");
        return Ok(());
    };
    // The test context strip.
    let mut context_edits: Vec<(String, String)> = Vec::new();
    ui.horizontal_wrapped(|ui| {
        ui.strong("Test context");
        for (name, kind) in &doc.table.schema.inputs {
            let current = doc.context.input(name).map(value_ron).unwrap_or_default();
            let text = doc.context_text.entry(name.clone()).or_insert(current);
            ui.label(format!("{name} ({})", kind.name()));
            let response = ui.add(egui::TextEdit::singleline(text).desired_width(90.0));
            if response.lost_focus() {
                context_edits.push((name.clone(), text.clone()));
            }
        }
        ui.label("seed");
        let mut seed = doc.seed;
        if ui.add(egui::DragValue::new(&mut seed)).changed() {
            doc.seed = seed;
            doc.evaluate();
        }
    });
    let mut context_changed = false;
    for (name, text) in context_edits {
        if doc.context.input(&name).map(value_ron).unwrap_or_default() == text {
            continue;
        }
        doc.set_input_text(&name, &text)?;
        context_changed = true;
    }
    if context_changed {
        doc.evaluate();
    }
    match &doc.outcome {
        Some(o) if o.used_catch_all => ui.label(format!("catch-all → {}", result_ron(&o.result))),
        Some(o) => ui.label(format!(
            "row {} → {}",
            o.row.as_deref().unwrap_or("?"),
            result_ron(&o.result)
        )),
        None => ui.weak("no result"),
    };
    let mut edits: Vec<Value> = Vec::new();
    // The options bar: every control runs the same set_options edit chooser.set_options runs.
    ui.horizontal_wrapped(|ui| {
        ui.strong("Options");
        let mut mode = doc.table.mode;
        egui::ComboBox::from_id_salt("tables-chooser-mode")
            .selected_text(format!("mode: {}", mode.name()))
            .show_ui(ui, |ui| {
                for choice in [Mode::First, Mode::Best, Mode::Weighted, Mode::All] {
                    ui.selectable_value(&mut mode, choice, choice.name());
                }
            });
        if mode != doc.table.mode {
            edits.push(json!({ "op": "set_options", "mode": mode.name() }));
        }
        let current_type = ron::to_string(&doc.table.result_type).unwrap_or_default();
        let response = ui
            .add(
                egui::TextEdit::singleline(&mut doc.result_type_text)
                    .desired_width(130.0)
                    .hint_text("result type"),
            )
            .on_hover_text("The result type as RON (Asset(\"clip\"), Value(Name), OutputOnly)");
        if response.lost_focus() && doc.result_type_text.trim() != current_type {
            edits.push(json!({ "op": "set_options", "result": doc.result_type_text.trim() }));
        }
        let current_catch_all = doc
            .table
            .catch_all
            .as_ref()
            .map(|c| result_ron(&c.result))
            .unwrap_or_default();
        let response = ui
            .add(
                egui::TextEdit::singleline(&mut doc.catch_all_text)
                    .desired_width(130.0)
                    .hint_text("catch-all"),
            )
            .on_hover_text("The catch-all result as RON; a blank box removes the catch-all");
        if response.lost_focus() {
            let text = doc.catch_all_text.trim().to_owned();
            if text.is_empty() {
                if doc.table.catch_all.is_some() {
                    edits.push(json!({ "op": "set_options", "catch_all": null }));
                }
            } else if text != current_catch_all {
                // Keep the catch-all's declared outputs; only the result changes here.
                let outputs: serde_json::Map<String, Value> = doc
                    .table
                    .catch_all
                    .iter()
                    .flat_map(|c| c.outputs.iter())
                    .map(|(k, v)| (k.clone(), regia_agent::choosers::value_json(v)))
                    .collect();
                edits.push(json!({ "op": "set_options", "catch_all": { "result": { "ron": text }, "outputs": outputs } }));
            }
        }
        let mut band_on = doc.table.near_best.is_some();
        if ui
            .checkbox(&mut band_on, "near-best band")
            .on_hover_text("Weighted: only candidates within this cost of the best draw")
            .changed()
        {
            edits.push(
                json!({ "op": "set_options", "near_best": if band_on { json!(doc.table.near_best.unwrap_or(0.0)) } else { json!(null) } }),
            );
        }
        if let Some(band) = doc.table.near_best {
            let mut value = band;
            if ui
                .add(egui::DragValue::new(&mut value).speed(0.01).range(0.0..=1000.0))
                .changed()
            {
                edits.push(json!({ "op": "set_options", "near_best": value }));
            }
        }
        let mut recency_on = doc.table.recency.is_some();
        if ui
            .checkbox(&mut recency_on, "recency")
            .on_hover_text("Weighted: multiply a recently picked row's weight")
            .changed()
        {
            edits.push(json!({
                "op": "set_options",
                "recency": if recency_on { json!({ "window": 1, "penalty": 0.0 }) } else { json!(null) },
            }));
        }
        if let Some(recency) = doc.table.recency {
            let (mut window, mut penalty) = (recency.window, recency.penalty);
            let a = ui
                .add(egui::DragValue::new(&mut window).range(1..=16).prefix("window "))
                .changed();
            let b = ui
                .add(egui::DragValue::new(&mut penalty).speed(0.01).range(0.0..=1.0).prefix("penalty "))
                .changed();
            if a || b {
                edits.push(json!({ "op": "set_options", "recency": { "window": window, "penalty": penalty } }));
            }
        }
    });
    // The catch-all's outputs map: one value box per declared output (blank = not set), each change one
    // set_options edit with the catch-all's result kept, as chooser.set_options takes it.
    let mut bad_output: Option<PanelError> = None;
    if let Some(catch_all) = doc.table.catch_all.clone()
        && !doc.table.schema.outputs.is_empty()
    {
        ui.horizontal_wrapped(|ui| {
            ui.strong("Catch-all outputs");
            for (name, kind) in &doc.table.schema.outputs {
                let current = catch_all.outputs.get(name).map(value_ron).unwrap_or_default();
                let text = doc
                    .outputs_text
                    .entry(name.clone())
                    .or_insert_with(|| current.clone());
                ui.label(format!("{name} ({})", kind.name()));
                let response = ui
                    .add(egui::TextEdit::singleline(text).desired_width(90.0))
                    .on_hover_text("The value the catch-all emits for this output as RON; blank = not set");
                if !response.lost_focus() || text.trim() == current {
                    continue;
                }
                let mut outputs: serde_json::Map<String, Value> = catch_all
                    .outputs
                    .iter()
                    .map(|(k, v)| (k.clone(), regia_agent::choosers::value_json(v)))
                    .collect();
                if text.trim().is_empty() {
                    outputs.remove(name);
                } else {
                    match parse_value(kind, text.trim()) {
                        Ok(value) => {
                            outputs.insert(name.clone(), regia_agent::choosers::value_json(&value));
                        }
                        Err(e) => {
                            bad_output = Some(err("bad_value", format!("output `{name}`: {e}")));
                            continue;
                        }
                    }
                }
                edits.push(json!({
                    "op": "set_options",
                    "catch_all": { "result": { "ron": result_ron(&catch_all.result) }, "outputs": outputs },
                }));
            }
        });
    }
    if let Some(e) = bad_output {
        // The box keeps what was typed (ADR-7716): clearing it made the box show the stored value
        // again, so the text to fix was gone. The table is unchanged and the error says why.
        return Err(e);
    }
    let columns = doc.table.columns.clone();
    // Only the rows in view are laid out (and their results printed, ADR-7306): a chooser table can
    // hold thousands of rows. Row 0 of the scroll range is the header.
    let row_count = doc.table.rows.len();
    let row_id = |doc: &ChooserDoc, index: usize| doc.table.rows.get(index).map(|r| r.id.clone());
    let row_height = ui.spacing().interact_size.y + 4.0;
    egui::ScrollArea::both().id_salt("tables-chooser-grid").show_rows(ui, row_height, row_count + 1, |ui, range| {
        egui::Grid::new("tables-chooser").striped(true).show(ui, |ui| {
            if range.start == 0 {
            ui.strong("row");
            ui.strong("on");
            for column in &columns {
                ui.vertical(|ui| {
                    ui.strong(&column.id);
                    ui.weak(format!(
                        "{} {}",
                        column.kind.ron_name(),
                        column.input.as_deref().or(column.output.as_deref()).unwrap_or("")
                    ));
                    ui.horizontal(|ui| {
                        let mut on = !column.disabled;
                        if ui.checkbox(&mut on, "").on_hover_text("Column enabled").changed() {
                            edits.push(json!({ "op": "set_column", "column": column.id, "disabled": !on }));
                        }
                        if ui.small_button("\u{1f5d9}").on_hover_text("Remove the column").clicked() {
                            edits.push(json!({ "op": "remove_column", "column": column.id }));
                        }
                    });
                });
            }
            ui.strong("result");
            ui.end_row();
            }
            for index in range.start.saturating_sub(1)..range.end.saturating_sub(1).min(row_count) {
                let Some((id, enabled, result)) = doc
                    .table
                    .rows
                    .get(index)
                    .map(|r| (r.id.clone(), r.enabled, result_ron(&r.result)))
                else {
                    continue;
                };
                let (id, enabled) = (&id, &enabled);
                let state = doc.row_state(id);
                let fill = if state == "selected" { SELECTED_FILL } else { egui::Color32::TRANSPARENT };
                ui.horizontal(|ui| {
                    egui::Frame::new().fill(fill).show(ui, |ui| {
                        ui.label(id).on_hover_text(state);
                    });
                    if ui.add_enabled(index > 0, egui::Button::new("▲").small()).clicked() {
                        let before = index.checked_sub(1).and_then(|i| row_id(doc, i));
                        edits.push(json!({ "op": "move_row", "row": id, "before": before }));
                    }
                    if ui.add_enabled(index + 1 < row_count, egui::Button::new("▼").small()).clicked() {
                        let before = row_id(doc, index + 2);
                        edits.push(json!({ "op": "move_row", "row": id, "before": before }));
                    }
                });
                let mut on = *enabled;
                if ui.checkbox(&mut on, "").changed() {
                    edits.push(json!({ "op": "set_row", "row": id, "enabled": on }));
                }
                for column in &columns {
                    let cell_state = doc.cell_state(id, &column.id);
                    let detail = doc.detail(id, &column.id);
                    let issue = doc.issue_at(id, &column.id).map(ToString::to_string);
                    let current = doc
                        .table
                        .row(id)
                        .map(|r| cell_ron(r.cell(&column.id)))
                        .unwrap_or_default();
                    let key = (id.clone(), column.id.clone());
                    let text = doc.cell_text.entry(key).or_insert(current.clone());
                    let mut frame = egui::Frame::new().fill(cell_state.fill()).inner_margin(2.0);
                    if issue.is_some() {
                        frame = frame.stroke(egui::Stroke::new(1.5, egui::Color32::from_rgb(240, 160, 40)));
                    }
                    frame.show(ui, |ui| {
                        let mut hover = detail;
                        if let Some(issue) = &issue {
                            hover = format!("{hover}\n{issue}");
                        }
                        let response = ui
                            .add(egui::TextEdit::singleline(text).desired_width(80.0))
                            .on_hover_text(hover);
                        if response.lost_focus() && *text != current {
                            edits.push(json!({ "op": "set_cell", "row": id, "column": column.id, "text": text.clone() }));
                        }
                    });
                }
                // The row's result, edited as RON text through the same set_row edit the agent's
                // chooser.set_row runs.
                let result_text = doc.result_text.entry(id.clone()).or_insert_with(|| result.clone());
                let response = ui
                    .add(egui::TextEdit::singleline(result_text).desired_width(90.0))
                    .on_hover_text("The row's result as RON (Value(\"run\"), Asset(\"a.ron\"), Nothing)");
                if response.lost_focus() && *result_text != result {
                    edits.push(json!({ "op": "set_row", "row": id, "result": { "ron": result_text.clone() } }));
                }
                if ui.small_button("\u{1f5d9}").on_hover_text("Remove the row").clicked() {
                    edits.push(json!({ "op": "remove_row", "row": id }));
                }
                ui.end_row();
            }
        });
    });
    // The add bars: one set_row / set_column edit each, exactly what chooser.set_row and
    // chooser.set_column run.
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut doc.new_row)
                .hint_text("new row id")
                .desired_width(120.0),
        );
        let id = doc.new_row.trim();
        let fresh = !id.is_empty() && doc.table.row(id).is_none();
        if ui
            .add_enabled(fresh, egui::Button::new("Add row"))
            .on_hover_text("Create the row with no result (Nothing)")
            .clicked()
        {
            edits.push(json!({ "op": "set_row", "row": id }));
            doc.new_row.clear();
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut doc.new_column_id)
                .hint_text("new column id")
                .desired_width(120.0),
        );
        egui::ComboBox::from_id_salt("tables-chooser-new-kind")
            .selected_text(doc.new_column_kind)
            .show_ui(ui, |ui| {
                for &kind in ADDABLE_COLUMN_KINDS {
                    if ui.selectable_label(doc.new_column_kind == kind, kind).clicked() {
                        doc.new_column_kind = kind;
                    }
                }
            });
        // `FloatDistance` takes its parameters here; the other kinds take none.
        let kind_text = if doc.new_column_kind == DISTANCE_KIND {
            let form = &mut doc.new_distance;
            ui.add(
                egui::DragValue::new(&mut form.influence)
                    .speed(0.01)
                    .range(0.0..=1000.0)
                    .prefix("influence "),
            );
            ui.add(
                egui::DragValue::new(&mut form.max_distance)
                    .speed(0.01)
                    .range(0.001..=1.0e6)
                    .prefix("max distance "),
            );
            let mut wrap_on = form.wrap.is_some();
            if ui
                .checkbox(&mut wrap_on, "wrap")
                .on_hover_text("A circular range (angles): the gap is the shorter way round")
                .changed()
            {
                form.wrap = wrap_on.then_some((-180.0, 180.0));
            }
            if let Some((lo, hi)) = &mut form.wrap {
                ui.add(egui::DragValue::new(lo).speed(0.1).prefix("from "));
                ui.add(egui::DragValue::new(hi).speed(0.1).prefix("to "));
            }
            ui.checkbox(&mut form.drop_beyond, "drop beyond")
                .on_hover_text("Also reject rows whose gap exceeds the max distance");
            form.kind_text()
        } else {
            doc.new_column_kind.to_owned()
        };
        let kind = parse_column_kind(&kind_text).ok();
        // A gate or rank column binds to a declared input that fits, an emit column to an
        // output; Weight binds to nothing. The names are copied out so the combo below can
        // borrow `doc` mutably.
        let fitting: Vec<String> = match kind.as_ref().map(|k| (k.class(), k)) {
            Some((class, k)) if class != ColumnClass::Draw => {
                let declared = if class == ColumnClass::Emit {
                    &doc.table.schema.outputs
                } else {
                    &doc.table.schema.inputs
                };
                declared
                    .iter()
                    .filter(|&(_, value_kind)| k.fits(value_kind))
                    .map(|(name, _)| name.clone())
                    .collect()
            }
            _ => Vec::new(),
        };
        doc.new_column_binding = doc.new_column_binding.min(fitting.len().saturating_sub(1));
        if !fitting.is_empty() {
            egui::ComboBox::from_id_salt("tables-chooser-new-binding")
                .selected_text(fitting[doc.new_column_binding].as_str())
                .show_ui(ui, |ui| {
                    for (at, name) in fitting.iter().enumerate() {
                        ui.selectable_label(doc.new_column_binding == at, name.as_str())
                            .clicked()
                            .then(|| doc.new_column_binding = at);
                    }
                });
        } else if kind.as_ref().is_some_and(|k| k.class() != ColumnClass::Draw) {
            ui.weak(format!("no declared {} fits", doc.new_column_kind));
        }
        let id = doc.new_column_id.trim();
        let fresh = !id.is_empty() && doc.table.column(id).is_none();
        let addable = fresh
            && kind
                .as_ref()
                .is_some_and(|k| k.class() == ColumnClass::Draw || !fitting.is_empty());
        if ui
            .add_enabled(addable, egui::Button::new("Add column"))
            .on_hover_text("Create the column bound to the chosen input or output")
            .clicked()
        {
            let (key, value) = match kind.as_ref().map(|k| k.class()) {
                Some(ColumnClass::Emit) => ("output", fitting[doc.new_column_binding].as_str()),
                _ => (
                    "input",
                    fitting
                        .get(doc.new_column_binding)
                        .map(String::as_str)
                        .unwrap_or(""),
                ),
            };
            let mut column = json!({ "id": id, "kind": kind_text });
            column[key] = json!(value);
            edits.push(json!({ "op": "set_column", "column": column }));
            doc.new_column_id.clear();
        }
    });
    for issue in &doc.issues {
        ui.colored_label(egui::Color32::from_rgb(230, 170, 80), issue.to_string());
    }
    for edit in edits {
        if let Err(e) = panel.chooser_edit(&edit) {
            // A refused cell keeps the table; the box shows the stored text again.
            if let Some(doc) = panel.chooser.as_mut() {
                doc.cell_text.clear();
            }
            return Err(e);
        }
    }
    Ok(())
}

// --------------------------------------------------------------------------------------- agent surface

fn with_panel<T>(
    world: &mut World,
    f: impl FnOnce(&mut TablesPanel, Option<&Path>) -> Result<T, PanelError>,
) -> Result<T, RpcError> {
    let project = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<TablesPanel>().unwrap_or_default();
    prepare_shared(world, &mut panel);
    let result = f(&mut panel, project.as_deref());
    match &result {
        Ok(_) => panel.error = None,
        Err(e) => panel.error = Some(e.clone()),
    }
    finish_shared(world, panel, None);
    result.map_err(|e| e.rpc(HINT))
}

/// [`with_panel`] for operations that need the table's schema (the Data tab).
fn with_panel_lookup<T>(
    world: &mut World,
    f: impl FnOnce(&mut TablesPanel, Option<&Path>, &SchemaLookup<'_>) -> Result<T, PanelError>,
) -> Result<T, RpcError> {
    let project = crate::mesh_source::project_root(world);
    let mut panel = world.remove_resource::<TablesPanel>().unwrap_or_default();
    prepare_shared(world, &mut panel);
    let result = f(&mut panel, project.as_deref(), &SchemaLookup::of(world));
    match &result {
        Ok(_) => panel.error = None,
        Err(e) => panel.error = Some(e.clone()),
    }
    finish_shared(world, panel, None);
    result.map_err(|e| e.rpc(HINT))
}

fn state(world: &World) -> Value {
    let mut value = world
        .get_resource::<TablesPanel>()
        .map_or_else(|| TablesPanel::default().json(), TablesPanel::json);
    if let Some(stack) = world.get_resource::<crate::command::UndoStack>() {
        value["shared_undo_depth"] = json!(stack.undo_depth());
        value["shared_redo_depth"] = json!(stack.redo_depth());
    }
    value
}

fn tab_param(params: &Value) -> Result<Option<TablesTab>, RpcError> {
    match params.get("tab").and_then(Value::as_str) {
        None => Ok(None),
        Some("curves") => Ok(Some(TablesTab::Curves)),
        Some("chooser") => Ok(Some(TablesTab::Chooser)),
        Some("data") => Ok(Some(TablesTab::Data)),
        Some(other) => {
            Err(PanelError::new("tables_panel_bad_value", format!("unknown tab `{other}`")).rpc(HINT))
        }
    }
}

fn panel_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")?.to_owned();
    with_panel_lookup(world, |panel, project, lookup| {
        panel.open_in(project, &path, lookup)
    })?;
    Ok(state(world))
}

fn panel_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(state(world))
}

fn panel_curve_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, _| {
        panel.tab = TablesTab::Curves;
        panel.curve_edit(params)
    })?;
    Ok(state(world))
}

fn panel_chooser_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, _| {
        panel.tab = TablesTab::Chooser;
        panel.chooser_edit(params)
    })?;
    Ok(state(world))
}

fn panel_set_context(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, _| {
        panel.tab = TablesTab::Chooser;
        panel.set_context(params)
    })?;
    Ok(state(world))
}

fn panel_undo(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let tab = tab_param(params)?;
    with_panel_lookup(world, |panel, _, lookup| panel.undo_in(tab, lookup))?;
    Ok(state(world))
}

fn panel_data_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel_lookup(world, |panel, _, lookup| {
        panel.tab = TablesTab::Data;
        let doc = panel
            .data
            .as_mut()
            .ok_or_else(|| err("no_table", "open a data table first"))?;
        data::data_edit(doc, params, lookup)
    })?;
    Ok(state(world))
}

fn panel_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let tab = tab_param(params)?;
    let written = with_panel(world, |panel, project| panel.save(project, tab))?;
    Ok(json!({ "written": written }))
}

const fn param(
    name: &'static str,
    kind: &'static str,
    required: bool,
    description: &'static str,
) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required,
        description,
    }
}

const STATE: &str = "{ tab, shared_undo_depth, shared_redo_depth, curves: { path, interp, generation, dirty, undo_depth, selected, selected_key, curves: [{ name, \
    pre, post, interp, keys }], issues }, chooser: { path, mode, generation, dirty, undo_depth, nested, inputs, context, seed, \
    columns, rows: [{ id, enabled, state, result, cells: [{ column, cell, state, cost, detail, issue }] }], outcome: { row, \
    used_catch_all, result, outputs, draw }, issues }, data: { path, format, row_type, schema, generation, dirty, undo_depth, \
    columns: [{ name, type_path, text, doc, known }], rows: [{ key, line, state, cells: [{ column, ron, text, state, issue }] }], \
    issues: [{ code, severity, message, key, column, line }], errors, warnings }, error, last_saved }";

const OPEN_PARAMS: &[ParamSchema] = &[param(
    "path",
    "string",
    true,
    "Table under assets/: .curves.ron, .curves.csv, .chooser.ron, .table.ron or .table.csv.",
)];
const CURVE_EDIT_PARAMS: &[ParamSchema] = &[
    param(
        "op",
        "string",
        true,
        "set_key | move_key | remove_key | set_tangent | set_curve | delete | select | import_csv",
    ),
    param("name", "string", false, "The curve."),
    param(
        "x",
        "number",
        false,
        "Key position (set_key, remove_key; move_key's new position).",
    ),
    param("y", "number", false, "Key value (set_key; move_key's new value)."),
    param("from_x", "number", false, "move_key: the key's current position."),
    param(
        "interp",
        "string",
        false,
        "step | linear | cubic (set_key, set_curve, import_csv).",
    ),
    param("in_tangent", "number", false, "set_key: incoming tangent."),
    param("out_tangent", "number", false, "set_key: outgoing tangent."),
    param(
        "pre",
        "string",
        false,
        "set_curve: hold | linear | cycle | cycle_offset | ping_pong.",
    ),
    param(
        "post",
        "string",
        false,
        "set_curve: extrapolation after the last key.",
    ),
    param("rename", "string", false, "set_curve: the curve's new name."),
    param("csv", "string", false, "import_csv: the whole table as CSV text."),
    param(
        "side",
        "string",
        false,
        "set_tangent: in | out | both (default both).",
    ),
    param(
        "slope",
        "number",
        false,
        "set_tangent: the tangent (value per unit x); null returns to the derived one.",
    ),
    param(
        "gesture",
        "number",
        false,
        "Edits with the same gesture id (one drag) are one undo step.",
    ),
];
const CHOOSER_EDIT_PARAMS: &[ParamSchema] = &[
    param(
        "op",
        "string",
        true,
        "set_cell | set_row | move_row | remove_row | set_column | remove_column | set_options",
    ),
    param(
        "row",
        "string",
        false,
        "The row id (set_row creates it when absent).",
    ),
    param(
        "column",
        "any",
        false,
        "The column id, or (set_column) a chooser.set_column definition object.",
    ),
    param(
        "value",
        "any",
        false,
        "set_cell: the cell as chooser.set_cell takes it (null = Any).",
    ),
    param(
        "text",
        "string",
        false,
        "set_cell: the cell as RON text (wins over value).",
    ),
    param(
        "enabled",
        "boolean",
        false,
        "set_row: whether the row takes part.",
    ),
    param(
        "result",
        "any",
        false,
        "set_row: the row's result in chooser.set_row's JSON forms ({ ron: \"...\" } parses RON); \
         set_options: the result type as RON text.",
    ),
    param(
        "cells",
        "object",
        false,
        "set_row: { column: value | null } cells to set (null = Any).",
    ),
    param("note", "string", false, "set_row: the row's note."),
    param(
        "before",
        "string",
        false,
        "move_row, set_row, set_column: the row or column it goes before (absent = the end).",
    ),
    param(
        "disabled",
        "boolean",
        false,
        "set_column: whether the column is skipped.",
    ),
    param(
        "mode",
        "string",
        false,
        "set_options: first | best | weighted | all.",
    ),
    param(
        "catch_all",
        "any",
        false,
        "set_options: { result, outputs? } or null (no catch-all).",
    ),
    param(
        "near_best",
        "number",
        false,
        "set_options: the Weighted best-cost band, or null.",
    ),
    param(
        "recency",
        "any",
        false,
        "set_options: { window, penalty } or null.",
    ),
];
const CONTEXT_PARAMS: &[ParamSchema] = &[
    param(
        "context",
        "object",
        false,
        "{ input: value | null } in chooser.evaluate's JSON forms.",
    ),
    param("seed", "number", false, "Seed of the weighted draw."),
    param("replace", "boolean", false, "Clear the other inputs first."),
];
const DATA_EDIT_PARAMS: &[ParamSchema] = &[
    param(
        "op",
        "string",
        true,
        "set_cell | clear_cell | insert_row | remove_row | rename_row | move_row",
    ),
    param(
        "key",
        "string",
        true,
        "The row key (insert_row: the new row's key).",
    ),
    param("column", "string", false, "set_cell, clear_cell: the field."),
    param(
        "text",
        "string",
        false,
        "set_cell: the cell as typed in the grid (CSV cell rules: plain text in a String field).",
    ),
    param(
        "ron",
        "string",
        false,
        "set_cell: the cell as a RON literal, stored as is (wins over text).",
    ),
    param("to", "string", false, "rename_row: the new key."),
    param(
        "before",
        "string",
        false,
        "insert_row, move_row: the row it goes before (absent = the end).",
    ),
    param(
        "cells",
        "object",
        false,
        "insert_row: { field: text } cells of the new row (fields left out take their default).",
    ),
];
const TAB_PARAMS: &[ParamSchema] = &[param(
    "tab",
    "string",
    false,
    "curves | chooser | data (default: the shown tab).",
)];

/// The Tables panel's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    let mut methods = vec![
        (
            MethodSchema {
                name: "editor.tables_panel_open",
                tool: "tables_panel_open",
                description: "Open a curve table, a chooser or a typed data table from assets/ in the Tables panel \
                    (spec 51 § 6, ADR-2132).",
                params: OPEN_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tables_panel_get",
                tool: "tables_panel_get",
                description: "The Tables panel: the open curve table, chooser and data table, the chooser's test context, \
                    each grid cell's Pass/Fail/Score state and the selected row.",
                params: &[],
                result: STATE,
                mutating: false,
                permission: Permission::Read,
            },
            panel_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tables_panel_curve_edit",
                tool: "tables_panel_curve_edit",
                description: "Edit the open curve table as the graph and grid do (in memory, undoable; \
                    editor.tables_panel_save writes it).",
                params: CURVE_EDIT_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_curve_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tables_panel_chooser_edit",
                tool: "tables_panel_chooser_edit",
                description: "Edit the open chooser as the grid does (cells, add, remove, enable and \
                    reorder rows, add, remove and disable columns, row results, mode and draw \
                    options), through the same edit functions chooser.* uses; the test context is \
                    evaluated again.",
                params: CHOOSER_EDIT_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_chooser_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tables_panel_data_edit",
                tool: "tables_panel_data_edit",
                description: "Edit the open data table as the Data grid does (cells, insert, remove, rename and \
                    reorder rows; in memory, undoable). An edit that would add a schema error is refused with \
                    its table_* code.",
                params: DATA_EDIT_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_data_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tables_panel_set_context",
                tool: "tables_panel_set_context",
                description: "Set the chooser's test context and seed; the grid's cell colors follow the new \
                    evaluation.",
                params: CONTEXT_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_set_context as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tables_panel_undo",
                tool: "tables_panel_undo",
                description: "Undo the last shared editor edit, reactivating its table document when needed.",
                params: TAB_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_undo as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tables_panel_save",
                tool: "tables_panel_save",
                description: "Write the open curve table, chooser or data table back to its file, atomically.",
                params: TAB_PARAMS,
                result: "{ written }",
                mutating: true,
                permission: Permission::Assets,
            },
            panel_save as Handler,
        ),
    ];
    methods.extend(exchange::methods());
    methods
}

// Shared history retains each opened document instance, including when another file replaces its tab.
enum TableDocument {
    Curves(Box<CurveDoc>),
    Chooser(Box<ChooserDoc>),
    Data(Box<DataDoc>),
}

impl TableDocument {
    fn id(&self) -> u64 {
        match self {
            Self::Curves(d) => d.history_id,
            Self::Chooser(d) => d.history_id,
            Self::Data(d) => d.history_id,
        }
    }
}

#[derive(Resource, Default)]
struct TableDocuments {
    next: u64,
    retired: BTreeMap<u64, TableDocument>,
}

fn prepare_shared(world: &World, panel: &mut TablesPanel) {
    panel.shared_depth = world
        .get_resource::<crate::command::UndoStack>()
        .map(|s| s.undo_depth());
}

fn finish_shared(world: &mut World, mut panel: TablesPanel, frame: Option<u64>) {
    let _span = tracing::trace_span!(target: "regia_editor", "table_shared_history").entered();
    if panel.shared_depth.is_some() {
        let mut docs = world.remove_resource::<TableDocuments>().unwrap_or_default();
        for doc in panel.retired.drain(..) {
            docs.retired.insert(doc.id(), doc);
        }
        let mut commands: Vec<Box<dyn crate::command::EditorCommand>> = Vec::new();
        let mut assign = |id: &mut u64| {
            if *id == 0 {
                docs.next += 1;
                *id = docs.next;
            }
        };
        if let Some(doc) = &mut panel.curves {
            assign(&mut doc.history_id);
            for table in doc.undo.drain(..) {
                commands.push(Box::new(TableEdit {
                    id: doc.history_id,
                    path: doc.path.clone(),
                    gesture: doc.last_gesture,
                    change: TableChange::Curves(table),
                }));
            }
        }
        if let Some(doc) = &mut panel.chooser {
            assign(&mut doc.history_id);
            for table in doc.undo.drain(..) {
                commands.push(Box::new(TableEdit {
                    id: doc.history_id,
                    path: doc.path.clone(),
                    gesture: None,
                    change: TableChange::Chooser(table),
                }));
            }
        }
        if let Some(doc) = &mut panel.data {
            assign(&mut doc.history_id);
            for op in doc.take_history() {
                commands.push(Box::new(TableEdit {
                    id: doc.history_id,
                    path: doc.path.clone(),
                    gesture: None,
                    change: TableChange::Data(op),
                }));
            }
        }
        world.insert_resource(docs);
        if commands.len() > 1 {
            let mut group = crate::command::CommandGroup::new("Edit table");
            for command in commands.drain(..) {
                group.push(command);
            }
            commands.push(Box::new(group));
        }
        let mut stack = world.resource_mut::<crate::command::UndoStack>();
        for command in commands {
            let continuous = (command.as_ref() as &dyn std::any::Any)
                .downcast_ref::<TableEdit>()
                .is_some_and(|c| c.gesture.is_some());
            if !continuous {
                stack.seal();
            }
            stack.push(command);
            if !continuous {
                stack.seal();
            }
        }
    }
    let undo = std::mem::take(&mut panel.history_requested);
    world.insert_resource(panel);
    if undo {
        if let Some(frame) = frame {
            crate::command::navigate_world_once(world, frame, false);
        } else {
            crate::command::undo_world(world);
        }
    }
}

enum TableChange {
    Curves(CurveTable),
    Chooser(ChooserTable),
    Data(data::DataOp),
}

struct TableEdit {
    id: u64,
    path: String,
    gesture: Option<u64>,
    change: TableChange,
}

impl TableEdit {
    fn swap(&mut self, world: &mut World) {
        let mut panel = world.remove_resource::<TablesPanel>().unwrap_or_default();
        let mut docs = world.remove_resource::<TableDocuments>().unwrap_or_default();
        let active = match self.change {
            TableChange::Curves(_) => panel
                .curves
                .take()
                .map(|doc| TableDocument::Curves(Box::new(doc))),
            TableChange::Chooser(_) => panel
                .chooser
                .take()
                .map(|doc| TableDocument::Chooser(Box::new(doc))),
            TableChange::Data(_) => panel.data.take().map(|doc| TableDocument::Data(Box::new(doc))),
        };
        if let Some(active) = active {
            docs.retired.insert(active.id(), active);
        }
        if let Some(mut document) = docs.retired.remove(&self.id) {
            match (&mut self.change, &mut document) {
                (TableChange::Curves(table), TableDocument::Curves(doc)) => {
                    std::mem::swap(table, &mut doc.table);
                    doc.generation += 1;
                    doc.dirty = true;
                    doc.issues = doc.table.validate();
                    doc.selected = doc.table.curves().first().map(|curve| curve.name.clone());
                    doc.selected_key = None;
                    doc.last_gesture = None;
                }
                (TableChange::Chooser(table), TableDocument::Chooser(doc)) => {
                    std::mem::swap(table, &mut doc.table);
                    doc.generation += 1;
                    doc.dirty = true;
                    doc.cell_text.clear();
                    doc.result_text.clear();
                    doc.outputs_text.clear();
                    doc.result_type_text = ron::to_string(&doc.table.result_type).unwrap_or_default();
                    doc.catch_all_text = doc
                        .table
                        .catch_all
                        .as_ref()
                        .map(|c| result_ron(&c.result))
                        .unwrap_or_default();
                    doc.evaluate();
                }
                (TableChange::Data(op), TableDocument::Data(doc)) => {
                    if let Err(error) = doc.replay_history(op, &SchemaLookup::of(world)) {
                        tracing::error!(?error, "table history replay failed");
                    }
                }
                _ => tracing::error!("table history document kind changed"),
            }
            match document {
                TableDocument::Curves(doc) => {
                    panel.curves = Some(*doc);
                    panel.tab = TablesTab::Curves;
                }
                TableDocument::Chooser(doc) => {
                    panel.chooser = Some(*doc);
                    panel.tab = TablesTab::Chooser;
                }
                TableDocument::Data(doc) => {
                    panel.data = Some(*doc);
                    panel.tab = TablesTab::Data;
                }
            }
        } else {
            tracing::error!(document = self.id, "table history document missing");
        }
        world.insert_resource(docs);
        world.insert_resource(panel);
    }
}

impl crate::command::EditorCommand for TableEdit {
    fn apply(&mut self, world: &mut World) {
        self.swap(world);
    }
    fn revert(&mut self, world: &mut World) {
        self.swap(world);
    }
    fn merge(&mut self, next: &dyn crate::command::EditorCommand) -> bool {
        (next as &dyn std::any::Any)
            .downcast_ref::<Self>()
            .is_some_and(|next| self.id == next.id && self.gesture.is_some() && self.gesture == next.gesture)
    }
    fn changes_level_state(&self) -> bool {
        false
    }
    fn touched(&self) -> Vec<crate::command::CommandTouch> {
        let property = match &self.change {
            TableChange::Curves(_) => "curves".to_owned(),
            TableChange::Chooser(_) => "chooser".to_owned(),
            TableChange::Data(op) => op.property(),
        };
        vec![crate::command::CommandTouch::asset(
            format!("assets/{}", self.path),
            Some(property),
        )]
    }
    fn description(&self) -> String {
        format!("Edit table {}", self.path)
    }
}

#[cfg(test)]
mod tests;
