// SPDX-License-Identifier: MPL-2.0
//! The asset audit panels (`docs/specs/32-asset-validation-and-audit.md` § 8): Problems, Reference
//! graph and Size map, over the validators and queries of `regia_asset_pipeline`.
//!
//! [`AssetPanels`] is the one model the egui panels and the agent methods
//! (`crate::asset_panels_agent`, `book/agents/asset-validators.md`) share, so a human and an agent
//! see the same run. The panels never change content: they read an asset set and report what the
//! user picks through [`AssetPanels::take_selected`], which the shell turns into a Content
//! Browser selection.

use regia_asset_pipeline::asset_audit::{
    AuditError, Column, GraphQuery, GraphResult, GroupBy, Side, SizeMap, SizeMapQuery, graph_query, size_map,
};
use regia_asset_pipeline::asset_graph::{AssetKind, AssetSet, DependencyGraph};
use regia_asset_pipeline::validation_rules::ValidationFile;
use regia_asset_pipeline::validators::{
    RunSettings, Scope, Severity, Trigger, ValidationReport, ValidatorRegistry,
};
use regia_ecs::prelude::*;

use crate::treemap::{Cell, squarify};

/// The severities in the order rows sort by, worst first.
pub const SEVERITIES: [Severity; 3] = [Severity::Error, Severity::Warning, Severity::Info];

/// The lowercase name of a severity (the panel's label and the agent's spelling).
#[must_use]
pub fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

fn severity_rank(severity: Severity) -> usize {
    SEVERITIES
        .iter()
        .position(|s| *s == severity)
        .unwrap_or(SEVERITIES.len())
}

/// Where the asset set of a panel came from, for the `editor.asset_panels_state` answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PanelSource {
    /// Nothing ran yet.
    #[default]
    None,
    /// The set came in an agent request.
    Request,
    /// The editor's loaded asset registry.
    Registry,
}

impl PanelSource {
    /// The agent's spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Request => "set",
            Self::Registry => "registry",
        }
    }
}

/// One finding of the Problems panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProblemRow {
    /// How serious.
    pub severity: Severity,
    /// The stable finding code.
    pub code: String,
    /// The validator that reported it.
    pub validator: String,
    /// The asset.
    pub asset: String,
    /// The text.
    pub text: String,
    /// What to do about it.
    pub hint: Option<String>,
}

/// The Problems panel: the last run's findings, sorted, and the filters over them.
///
/// The list can be grouped by source (the validator or system that reported a row) under
/// collapsible headers, worst group first, and identical consecutive rows show once with a count
/// (ADR-7789).
#[derive(Debug, Clone, PartialEq)]
pub struct ProblemsState {
    /// The last report.
    pub report: Option<ValidationReport>,
    /// Every finding: worst first, then asset, validator and the validator's own order.
    pub rows: Vec<ProblemRow>,
    /// Severities hidden by the filter.
    pub hidden_severities: Vec<Severity>,
    /// Show one validator only.
    pub validator: Option<String>,
    /// The scope of the last run, which *Re-run* repeats.
    pub last_scope: Option<Scope>,
    /// List the rows under one collapsible header per source (validator or system).
    pub grouped: bool,
    /// Show identical consecutive rows once, with a count.
    pub collapse_duplicates: bool,
    /// Sources whose group is collapsed.
    pub closed_groups: std::collections::BTreeSet<String>,
    /// The row last clicked (an index into [`Self::rows`]): *Copy selected* copies it.
    pub selected_row: Option<usize>,
    /// The egui frame whose control changes were applied ([`Self::apply_toggles`]).
    pub toggled_frame: Option<u64>,
}

/// One change a Problems panel control asks for. The panel applies them once per egui frame: a
/// second UI pass of the frame sees the same click and would flip the setting back (review
/// sprint-w12-rev-g5 #6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProblemToggle {
    /// Show (`true`) or hide a severity.
    Severity(Severity, bool),
    /// Group by source on or off.
    Grouped(bool),
    /// Collapse duplicates on or off.
    CollapseDuplicates(bool),
    /// Open or collapse the group of this source.
    Group(String),
}

impl Default for ProblemsState {
    fn default() -> Self {
        Self {
            report: None,
            rows: Vec::new(),
            hidden_severities: Vec::new(),
            validator: None,
            last_scope: None,
            grouped: false,
            collapse_duplicates: true,
            closed_groups: std::collections::BTreeSet::new(),
            selected_row: None,
            toggled_frame: None,
        }
    }
}

impl ProblemsState {
    /// Applies the control changes of egui frame `frame`, once: a second call for the same frame
    /// (egui's second UI pass) changes nothing. Returns whether it applied any.
    pub fn apply_toggles(&mut self, toggles: Vec<ProblemToggle>, frame: u64) -> bool {
        if toggles.is_empty() || self.toggled_frame == Some(frame) {
            return false;
        }
        self.toggled_frame = Some(frame);
        for toggle in toggles {
            match toggle {
                ProblemToggle::Severity(severity, show) => self.show_severity(severity, show),
                ProblemToggle::Grouped(on) => self.grouped = on,
                ProblemToggle::CollapseDuplicates(on) => self.collapse_duplicates = on,
                ProblemToggle::Group(source) => self.toggle_group(&source),
            }
        }
        true
    }
}

/// A run of identical consecutive findings: the first row, its index in [`ProblemsState::rows`]
/// and how many rows it stands for.
pub type ProblemRun<'a> = (&'a ProblemRow, usize, usize);

/// One line of the Problems list as drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemItem<'a> {
    /// A source's header (grouped view).
    Header {
        /// The validator or system.
        source: &'a str,
        /// Its rows after the filters (a collapsed run counts once).
        rows: usize,
        /// Its worst severity.
        worst: Severity,
        /// Whether its rows are listed.
        open: bool,
    },
    /// A finding: the row, its index in [`ProblemsState::rows`] and how many identical consecutive
    /// rows it stands for.
    Row(&'a ProblemRow, usize, usize),
}

impl ProblemsState {
    /// Takes in a report: the findings of every row, sorted (asset, validator and finding order
    /// come sorted from the runner, so a stable sort by severity keeps them).
    pub fn set_report(&mut self, report: ValidationReport) {
        let mut rows: Vec<ProblemRow> = report
            .results
            .iter()
            .flat_map(|row| {
                row.findings.iter().map(|finding| ProblemRow {
                    severity: finding.severity,
                    code: finding.code.clone(),
                    validator: row.validator.clone(),
                    asset: row.asset.clone(),
                    text: finding.text.clone(),
                    hint: finding.hint.clone(),
                })
            })
            .collect();
        rows.sort_by_key(|row| severity_rank(row.severity));
        self.rows = rows;
        self.selected_row = None;
        self.report = Some(report);
    }

    /// The rows the filters let through.
    #[must_use]
    pub fn visible(&self) -> Vec<&ProblemRow> {
        self.rows
            .iter()
            .filter(|row| !self.hidden_severities.contains(&row.severity))
            .filter(|row| self.validator.as_ref().is_none_or(|v| *v == row.validator))
            .collect()
    }

    /// The rows the filters let through with their indices in [`Self::rows`], identical
    /// consecutive rows folded into the first with a count when [`Self::collapse_duplicates`] is on.
    #[must_use]
    pub fn visible_runs(&self) -> Vec<ProblemRun<'_>> {
        let mut runs: Vec<ProblemRun<'_>> = Vec::new();
        for (index, row) in self.rows.iter().enumerate() {
            if self.hidden_severities.contains(&row.severity)
                || self.validator.as_ref().is_some_and(|v| *v != row.validator)
            {
                continue;
            }
            match runs.last_mut() {
                Some((last, _, count)) if self.collapse_duplicates && *last == row => *count += 1,
                _ => runs.push((row, index, 1)),
            }
        }
        runs
    }

    /// The list as drawn: [`Self::visible_runs`], or grouped by source (worst severity first, then
    /// name) with a header per source and its rows when the group is open.
    #[must_use]
    pub fn items(&self) -> Vec<ProblemItem<'_>> {
        let runs = self.visible_runs();
        if !self.grouped {
            return runs
                .into_iter()
                .map(|(row, index, count)| ProblemItem::Row(row, index, count))
                .collect();
        }
        let mut groups: std::collections::BTreeMap<&str, Vec<ProblemRun<'_>>> =
            std::collections::BTreeMap::new();
        for run in runs {
            groups.entry(run.0.validator.as_str()).or_default().push(run);
        }
        let mut order: Vec<(&str, Vec<ProblemRun<'_>>)> = groups.into_iter().collect();
        let worst = |rows: &[ProblemRun<'_>]| {
            rows.iter()
                .map(|(row, _, _)| severity_rank(row.severity))
                .min()
                .unwrap_or(SEVERITIES.len())
        };
        order.sort_by_key(|(name, rows)| (worst(rows), *name));
        let mut items = Vec::new();
        for (source, rows) in order {
            let open = !self.closed_groups.contains(source);
            items.push(ProblemItem::Header {
                source,
                rows: rows.len(),
                worst: SEVERITIES.get(worst(&rows)).copied().unwrap_or(Severity::Info),
                open,
            });
            if open {
                items.extend(
                    rows.into_iter()
                        .map(|(row, index, count)| ProblemItem::Row(row, index, count)),
                );
            }
        }
        items
    }

    /// Opens or collapses the group of `source`.
    pub fn toggle_group(&mut self, source: &str) {
        if !self.closed_groups.remove(source) {
            self.closed_groups.insert(source.to_owned());
        }
    }

    /// What *Copy all* copies: the drawn list, one line each (a header as `[source] (N)`, a
    /// collapsed run with ` (×N)`).
    #[must_use]
    pub fn copy_all_text(&self) -> String {
        let lines: Vec<String> = self
            .items()
            .into_iter()
            .map(|item| match item {
                ProblemItem::Header { source, rows, .. } => format!("[{source}] ({rows})"),
                ProblemItem::Row(row, _, count) => {
                    let mut line = problem_line(row);
                    if count > 1 {
                        line.push_str(&format!(" (×{count})"));
                    }
                    line
                }
            })
            .collect();
        lines.join("\n")
    }

    /// What *Copy selected* copies: the last clicked row, with its hint.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let row = self.rows.get(self.selected_row?)?;
        let mut text = problem_line(row);
        if let Some(hint) = &row.hint {
            text.push_str(&format!("\n  hint: {hint}"));
        }
        Some(text)
    }

    /// The validators with a finding, sorted (the filter's choices).
    #[must_use]
    pub fn validators(&self) -> Vec<String> {
        let mut names: Vec<String> = self.rows.iter().map(|row| row.validator.clone()).collect();
        names.sort();
        names.dedup();
        names
    }

    /// Shows or hides a severity.
    pub fn show_severity(&mut self, severity: Severity, show: bool) {
        self.hidden_severities.retain(|s| *s != severity);
        if !show {
            self.hidden_severities.push(severity);
        }
    }
}

/// The Reference graph panel: the current query, its answer and the history of queries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReferenceGraphState {
    /// The query on screen.
    pub query: GraphQuery,
    /// Its answer.
    pub result: Option<GraphResult>,
    /// Every query shown, oldest first.
    history: Vec<GraphQuery>,
    /// Where [`Self::query`] is in `history`.
    cursor: usize,
}

impl ReferenceGraphState {
    /// Runs `query` and, when it answers, records it after the current history position (a
    /// forward history is dropped, as a browser does). An error leaves the panel as it was.
    pub fn show(&mut self, set: &AssetSet, query: GraphQuery) -> Result<(), AuditError> {
        let result = run_graph(set, &query)?;
        self.history.truncate((self.cursor + 1).min(self.history.len()));
        if self.history.last() != Some(&query) {
            self.history.push(query.clone());
        }
        self.cursor = self.history.len().saturating_sub(1);
        self.query = query;
        self.result = Some(result);
        Ok(())
    }

    /// Whether there is an earlier query.
    #[must_use]
    pub fn can_back(&self) -> bool {
        self.cursor > 0
    }

    /// Whether there is a later query.
    #[must_use]
    pub fn can_forward(&self) -> bool {
        self.cursor + 1 < self.history.len()
    }

    /// Goes one query back (`-1`) or forward (`1`). False when there is none.
    pub fn step(&mut self, set: &AssetSet, delta: isize) -> Result<bool, AuditError> {
        let Some(target) = self
            .cursor
            .checked_add_signed(delta)
            .filter(|t| *t < self.history.len())
        else {
            return Ok(false);
        };
        let query = self.history[target].clone();
        self.result = Some(run_graph(set, &query)?);
        self.query = query;
        self.cursor = target;
        Ok(true)
    }

    /// Re-centers the graph on `asset`.
    pub fn recenter(&mut self, set: &AssetSet, asset: &str) -> Result<(), AuditError> {
        let mut query = self.query.clone();
        query.roots = vec![asset.to_owned()];
        query.pages.clear();
        self.show(set, query)
    }

    /// Shows the next chunk of a column (the "more" node); the last chunk wraps to the first.
    pub fn more(&mut self, set: &AssetSet, side: Side, hop: u32) -> Result<(), AuditError> {
        let Some(column) = self.column(side, hop) else {
            return Ok(());
        };
        let next = (column.page + 1) % column.pages.max(1);
        let mut query = self.query.clone();
        query.pages.retain(|(s, h, _)| !(*s == side && *h == hop));
        query.pages.push((side, hop, next));
        self.show(set, query)
    }

    /// Adds or removes a kind from the type filter.
    pub fn toggle_kind(&mut self, set: &AssetSet, kind: AssetKind) -> Result<(), AuditError> {
        let mut query = self.query.clone();
        if let Some(at) = query.kinds.iter().position(|k| *k == kind) {
            query.kinds.remove(at);
        } else {
            query.kinds.push(kind);
            query.kinds.sort();
        }
        query.pages.clear();
        self.show(set, query)
    }

    /// One column of the answer.
    #[must_use]
    pub fn column(&self, side: Side, hop: u32) -> Option<&Column> {
        self.result
            .as_ref()?
            .columns
            .iter()
            .find(|column| column.side == side && column.hop == hop)
    }
}

fn run_graph(set: &AssetSet, query: &GraphQuery) -> Result<GraphResult, AuditError> {
    graph_query(set, &DependencyGraph::build(set), query)
}

/// The Size map panel: the query, its rows and the treemap of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SizeMapState {
    /// The query on screen.
    pub query: SizeMapQuery,
    /// Its answer.
    pub result: Option<SizeMap>,
    /// The layout of the rows in the last drawn rectangle.
    pub cells: Vec<Cell>,
    /// The rectangle `cells` fills.
    pub area: (f32, f32),
}

/// What a point of the treemap shows (the panel's hover text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeHover {
    /// The row's name.
    pub name: String,
    /// Bytes.
    pub bytes: u64,
    /// Parts per ten thousand of the closure.
    pub share_bp: u32,
    /// What deleting it saves.
    pub exclusive_bytes: u64,
}

impl SizeMapState {
    /// Runs `query`. An error leaves the panel as it was.
    pub fn show(&mut self, set: &AssetSet, query: SizeMapQuery) -> Result<(), AuditError> {
        let result = size_map(set, &DependencyGraph::build(set), &query)?;
        self.query = query;
        self.result = Some(result);
        self.cells.clear();
        self.area = (0.0, 0.0);
        Ok(())
    }

    /// Lays the rows out in a `w` × `h` rectangle (a no-op when it already is).
    pub fn layout(&mut self, w: f32, h: f32) {
        if self.area == (w, h) && !self.cells.is_empty() {
            return;
        }
        let sizes: Vec<u64> = self
            .result
            .iter()
            .flat_map(|map| map.rows.iter().map(|row| row.bytes))
            .collect();
        self.cells = squarify(&sizes, w, h);
        self.area = (w, h);
    }

    /// What the treemap shows at a point.
    #[must_use]
    pub fn hover(&self, x: f32, y: f32) -> Option<SizeHover> {
        let map = self.result.as_ref()?;
        let cell = self.cells.iter().find(|cell| cell.contains(x, y))?;
        let row = map.rows.get(cell.index)?;
        let share_bp = if map.total_bytes == 0 {
            0
        } else {
            (u128::from(row.bytes) * 10_000 / u128::from(map.total_bytes)) as u32
        };
        Some(SizeHover {
            name: row.name.clone(),
            bytes: row.bytes,
            share_bp,
            exclusive_bytes: row.exclusive_bytes,
        })
    }
}

/// The model of the three panels.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct AssetPanels {
    /// The Problems panel.
    pub problems: ProblemsState,
    /// The Reference graph panel.
    pub graph: ReferenceGraphState,
    /// The Size map panel.
    pub size_map: SizeMapState,
    /// Where the last set came from.
    pub source: PanelSource,
    /// The last error a panel action hit (shown in the panels, cleared by the next action).
    pub error: Option<String>,
    /// The last audit CSV the Content Browser's *Audit to CSV* wrote.
    pub last_csv: Option<String>,
    selected: Option<String>,
}

impl AssetPanels {
    /// Runs the validators of `file` over `set` with `scope`: the panel's *Validate* and
    /// *Re-run*. A manual run; the report fills the Problems panel.
    pub fn validate(&mut self, set: &AssetSet, file: &ValidationFile, scope: Scope) {
        let _span = tracing::info_span!("validate_run", trigger = "panel").entered();
        let settings = RunSettings {
            scope: scope.clone(),
            trigger: Trigger::Manual,
            exclude: file.exclude.clone(),
            ..RunSettings::default()
        };
        let report = ValidatorRegistry::with_rules(file).run_with(set, &settings);
        self.problems.set_report(report);
        self.problems.last_scope = Some(scope);
    }

    /// Records that the user picked an asset. The panels never change content: this is the
    /// selection event the shell hands to the Content Browser.
    pub fn select(&mut self, asset: impl Into<String>) {
        self.selected = Some(asset.into());
    }

    /// The asset picked since the last call.
    pub fn take_selected(&mut self) -> Option<String> {
        self.selected.take()
    }

    /// Opens the size map of the graph's roots ("open size map").
    pub fn open_size_map(&mut self, set: &AssetSet) -> Result<(), AuditError> {
        let query = SizeMapQuery {
            roots: self.graph.query.roots.clone(),
            group_by: GroupBy::Asset,
            ..self.size_map.query.clone()
        };
        self.size_map.show(set, query)
    }
}

/// The editor's loaded registry as an asset set, when one is loaded.
#[must_use]
pub fn registry_set(world: &World) -> Option<AssetSet> {
    world
        .get_resource::<regia_asset::registry::LoadedAssetRegistry>()
        .map(|loaded| {
            let set = AssetSet::from_registry(&loaded.registry);
            // Layouts are raw files: their component `use` edges come from the files (ADR-3147).
            match world
                .get_resource::<crate::agent_ext::EditorProject>()
                .and_then(|p| p.root.clone())
            {
                Some(root) => set.with_ui_layouts(&root.join("assets")),
                None => set,
            }
        })
}

/// The project's `config/validation.ron`, or the defaults; a file that does not load is an error
/// the panel shows.
pub fn project_rules(world: &World) -> Result<ValidationFile, String> {
    let Some(root) = crate::mesh_source::project_root(world) else {
        return Ok(ValidationFile::default());
    };
    let path = root.join("config").join("validation.ron");
    if !path.is_file() {
        return Ok(ValidationFile::default());
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    ValidationFile::from_ron(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// A menu action of the Content Browser (spec 32 § 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetAction {
    /// Validate these paths (a folder path gathers what is under it).
    Validate(String),
    /// Validate the asset and what it depends on.
    ValidateWithDependencies(String),
    /// Open the reference graph on the asset.
    ReferenceGraph(String),
    /// Open the size map of the asset.
    SizeMap(String),
    /// Write the audit CSV of the asset or folder to `.regia/audit/`.
    AuditCsv(String),
    /// Validate the whole project.
    ValidateProject,
}

/// `folder` (a Content Browser folder, `assets/props` or `props`) as the set spells it: the set's
/// paths may carry the `assets/` prefix or not (ADR-7725).
#[must_use]
pub fn folder_in_set(set: &AssetSet, folder: &str) -> String {
    let folder = folder.trim_end_matches('/');
    let under = |prefix: &str| {
        let prefix = format!("{prefix}/");
        set.assets.iter().any(|asset| asset.path.starts_with(&prefix))
    };
    if under(folder) {
        return folder.to_owned();
    }
    match folder.strip_prefix("assets/") {
        Some(bare) if under(bare) => bare.to_owned(),
        _ => folder.to_owned(),
    }
}

/// Every asset of `set` under `folder` (recursive), in set order: the roots a folder's Reference
/// graph and Size map start from (ADR-7725).
#[must_use]
pub fn folder_assets(set: &AssetSet, folder: &str) -> Vec<String> {
    let prefix = format!("{}/", folder_in_set(set, folder));
    set.assets
        .iter()
        .filter(|asset| asset.path.starts_with(&prefix))
        .map(|asset| asset.path.clone())
        .collect()
}

/// `path`'s roots: itself when the set has it, else the assets under it as a folder (`true`: the
/// folder was expanded, which raises the reference graph's root cap).
fn roots_of(set: &AssetSet, path: &str) -> (Vec<String>, bool) {
    if set.get(path).is_some() {
        return (vec![path.to_owned()], false);
    }
    let under = folder_assets(set, path);
    if under.is_empty() {
        (vec![path.to_owned()], false)
    } else {
        (under, true)
    }
}

/// Carries out a menu action on the editor's registry and answers the toast text. Every panel the
/// action fills reports its own result; the toast says where it went.
pub fn run_action(world: &mut World, action: AssetAction) -> String {
    let Some(set) = registry_set(world) else {
        return "no asset registry is loaded: cook the project first".to_owned();
    };
    let rules = match project_rules(world) {
        Ok(rules) => rules,
        Err(err) => return err,
    };
    let root = crate::mesh_source::project_root(world);
    let mut panels = world.get_resource_or_insert_with(AssetPanels::default);
    panels.source = PanelSource::Registry;
    panels.error = None;
    match action {
        AssetAction::Validate(path) => {
            let scope = if set.get(&path).is_some() {
                Scope::Assets(vec![path])
            } else {
                Scope::Folder {
                    path: folder_in_set(&set, &path),
                    recursive: true,
                }
            };
            panels.validate(&set, &rules, scope);
            problems_toast(&panels)
        }
        AssetAction::ValidateWithDependencies(path) => {
            panels.validate(&set, &rules, Scope::WithDependencies(vec![path]));
            problems_toast(&panels)
        }
        AssetAction::ValidateProject => {
            panels.validate(&set, &rules, Scope::Project);
            problems_toast(&panels)
        }
        AssetAction::ReferenceGraph(path) => {
            let (roots, folder_expanded) = roots_of(&set, &path);
            let query = GraphQuery {
                roots,
                folder_expanded,
                ..panels.graph.query.clone()
            };
            match panels.graph.show(
                &set,
                GraphQuery {
                    pages: Vec::new(),
                    ..query
                },
            ) {
                Ok(()) => format!("reference graph of {path}"),
                Err(err) => report_error(&mut panels, &err),
            }
        }
        AssetAction::SizeMap(path) => {
            let query = SizeMapQuery {
                roots: roots_of(&set, &path).0,
                ..panels.size_map.query.clone()
            };
            match panels.size_map.show(&set, query) {
                Ok(()) => format!("size map of {path}"),
                Err(err) => report_error(&mut panels, &err),
            }
        }
        AssetAction::AuditCsv(path) => {
            let selection: Vec<String> = if set.get(&path).is_some() {
                vec![path.clone()]
            } else {
                folder_assets(&set, &path)
            };
            let graph = DependencyGraph::build(&set);
            let csv = regia_asset_pipeline::asset_audit::audit_csv(&set, &graph, &selection);
            let toast = match root.as_deref().map(|root| write_audit(root, &path, &csv)) {
                Some(Ok(file)) => format!("audit written to {}", file.display()),
                Some(Err(err)) => format!("audit not written: {err}"),
                None => "no project is open".to_owned(),
            };
            panels.last_csv = Some(csv);
            toast
        }
    }
}

fn report_error(panels: &mut AssetPanels, err: &AuditError) -> String {
    let text = format!("{}: {err}", err.code());
    panels.error = Some(text.clone());
    text
}

fn problems_toast(panels: &AssetPanels) -> String {
    let rows = panels.problems.rows.len();
    format!("validation: {rows} finding(s), see Problems")
}

/// Writes the CSV under `.regia/audit/`, temp file then rename.
fn write_audit(root: &std::path::Path, path: &str, csv: &str) -> std::io::Result<std::path::PathBuf> {
    let dir = root.join(".regia/audit");
    std::fs::create_dir_all(&dir)?;
    let stem: String = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let file = dir.join(format!("{stem}.csv"));
    let tmp = file.with_extension("csv.tmp");
    std::fs::write(&tmp, csv)?;
    std::fs::rename(&tmp, &file)?;
    Ok(file)
}

// ---------------------------------------------------------------------------
// egui
// ---------------------------------------------------------------------------

/// What a panel's frame asked for (actions run after the panel's borrow of the model ends).
enum GraphAction {
    Back,
    Forward,
    Depth(Side, u32),
    Width(u32),
    Kind(AssetKind),
    More(Side, u32),
    Recenter(String),
    OpenSizeMap,
}

/// A human size.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// One finding as one line: `<severity> <code> <asset>: <text>` (the CLI's form).
#[must_use]
pub fn problem_line(row: &ProblemRow) -> String {
    format!(
        "{} {} {}: {}",
        severity_name(row.severity),
        row.code,
        row.asset,
        row.text
    )
}

/// The entity GUIDs a finding's text names (each one a link that selects the entity).
#[must_use]
pub fn problem_entities(row: &ProblemRow) -> Vec<String> {
    crate::log_links::parse_links(&row.text)
        .into_iter()
        .filter(|link| link.target == crate::log_links::LinkTarget::Entity)
        .map(|link| link.path().to_owned())
        .collect()
}

/// The Problems panel.
pub fn problems_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut panels = world.remove_resource::<AssetPanels>().unwrap_or_default();
    let mut rerun = false;
    let mut copy = None;
    // Toggle changes are applied once per frame, after the list (ProblemToggle).
    let mut toggles: Vec<ProblemToggle> = Vec::new();
    ui.horizontal_wrapped(|ui| {
        rerun = ui.button("Re-run").clicked();
        for severity in SEVERITIES {
            let mut show = !panels.problems.hidden_severities.contains(&severity);
            if ui.checkbox(&mut show, severity_name(severity)).changed() {
                toggles.push(ProblemToggle::Severity(severity, show));
            }
        }
        let current = panels.problems.validator.clone();
        egui::ComboBox::from_id_salt("problems-validator")
            .selected_text(current.as_deref().unwrap_or("all validators"))
            .show_ui(ui, |ui| {
                if ui.selectable_label(current.is_none(), "all validators").clicked() {
                    panels.problems.validator = None;
                }
                for name in panels.problems.validators() {
                    let chosen = current.as_deref() == Some(name.as_str());
                    if ui.selectable_label(chosen, &name).clicked() {
                        panels.problems.validator = Some(name);
                    }
                }
            });
        let mut grouped = panels.problems.grouped;
        if ui
            .checkbox(&mut grouped, "Group by source")
            .on_hover_text("One collapsible group per validator or system")
            .changed()
        {
            toggles.push(ProblemToggle::Grouped(grouped));
        }
        let mut collapse = panels.problems.collapse_duplicates;
        if ui
            .checkbox(&mut collapse, "Collapse duplicates")
            .on_hover_text("Identical consecutive findings show once, with a count")
            .changed()
        {
            toggles.push(ProblemToggle::CollapseDuplicates(collapse));
        }
        if ui.button("Copy all").clicked() {
            copy = Some(panels.problems.copy_all_text());
        }
        if ui
            .add_enabled(
                panels.problems.selected_row.is_some(),
                egui::Button::new("Copy selected"),
            )
            .clicked()
        {
            copy = panels.problems.selected_text();
        }
    });
    if let Some(text) = copy {
        ui.ctx().copy_text(text);
    }
    // Diagnostic colors from the theme (Preferences ▸ Theme ▸ Diagnostic colors, ADR-7838).
    let theme = crate::theme::ui_tokens(ui.ctx());
    let severity_color = |severity: Severity| {
        let level = match severity {
            Severity::Error => crate::theme::DiagnosticLevel::Error,
            Severity::Warning => crate::theme::DiagnosticLevel::Warning,
            Severity::Info => crate::theme::DiagnosticLevel::Info,
        };
        crate::theme::diagnostic_color(&theme, level)
    };
    if let Some(error) = &panels.error {
        ui.colored_label(severity_color(Severity::Error), error);
    }
    match &panels.problems.report {
        None => {
            ui.label("Nothing validated yet: use Validate in the Content Browser, or Re-run.");
        }
        Some(report) => {
            ui.label(format!(
                "{} passed, {} failed, {} with warnings, {} not validated",
                report.summary.passed,
                report.summary.failed,
                report.summary.warnings,
                report.summary.not_validated,
            ));
        }
    }
    let mut picked = None;
    let mut toggled = None;
    let mut entity = None;
    let mut fix: Option<String> = None;
    // Only the rows in view are laid out (ADR-7306): a project-wide validation can list tens of
    // thousands of findings.
    let items = panels.problems.items();
    let selected = panels.problems.selected_row;
    let row_height = ui.spacing().interact_size.y;
    egui::ScrollArea::vertical().show_rows(ui, row_height, items.len(), |ui, range| {
        for item in &items[range] {
            match *item {
                ProblemItem::Header {
                    source,
                    rows,
                    worst,
                    open,
                } => {
                    let mark = if open { "▼" } else { "▶" };
                    let response = ui
                        .selectable_label(open, format!("{mark} {source} ({rows})"))
                        .on_hover_text(format!("worst: {}", severity_name(worst)));
                    if response.clicked() {
                        toggled = Some(source.to_owned());
                    }
                }
                ProblemItem::Row(row, index, count) => {
                    ui.horizontal(|ui| {
                        if panels.problems.grouped {
                            ui.add_space(row_height);
                        }
                        let mut line = problem_line(row);
                        if count > 1 {
                            line = format!("×{count} {line}");
                        }
                        // A missing reference gets the fixer's buttons in front of its line (ADR-7822).
                        let missing = (row.code == "validate_reference_missing")
                            .then(|| crate::content_actions::missing_path_of(&row.text))
                            .flatten();
                        if let Some(missing) = missing {
                            missing_reference_buttons(ui, missing, &mut fix);
                        }
                        let response = ui.selectable_label(
                            selected == Some(index),
                            egui::RichText::new(line).color(severity_color(row.severity)),
                        );
                        let response = match &row.hint {
                            Some(hint) => response.on_hover_text(hint),
                            None => response,
                        };
                        if response.clicked() {
                            picked = Some((row.asset.clone(), index));
                        }
                        for guid in problem_entities(row) {
                            if ui
                                .link(format!("entity {}", &guid[..8]))
                                .on_hover_text(format!("Select and frame entity {guid}"))
                                .clicked()
                            {
                                entity = Some(guid);
                            }
                        }
                    });
                }
            }
        }
    });
    drop(items);
    if let Some(source) = toggled {
        toggles.push(ProblemToggle::Group(source));
    }
    if panels
        .problems
        .apply_toggles(toggles, ui.ctx().cumulative_frame_nr())
    {
        // The list is laid out from the new settings on the next frame.
        ui.ctx().request_repaint();
    }
    if let Some((asset, index)) = picked {
        panels.problems.selected_row = Some(index);
        panels.select(asset);
    }
    world.insert_resource(panels);
    if let Some(guid) = entity
        && let Err(err) = crate::log_links::select_entity_by_guid(world, &guid)
    {
        world.resource_mut::<AssetPanels>().error = Some(err.message);
    }
    if rerun {
        rerun_problems(world);
    }
    // The level's configuration warnings (a second source, ADR-7772).
    entity_warnings_section(ui, world);
    // The open material graphs' compile errors (a third source, w13 P-185).
    graph_errors_section(ui, world);
    // The open logic graph's issues, each linked to its node (ADR-8021).
    crate::logic_graph_editor::problems::problems_section(ui, world);
    if let Some(missing) = fix {
        crate::content_actions::open_picker(world, crate::content_actions::PickerPurpose::Retarget, &missing);
    }
}

/// Repeats the last validation over the registry (the whole project the first time).
pub fn rerun_problems(world: &mut World) {
    let Some(set) = registry_set(world) else {
        world.get_resource_or_insert_with(AssetPanels::default).error =
            Some("registry_not_loaded: cook the project first".to_owned());
        return;
    };
    let rules = project_rules(world);
    let mut panels = world.get_resource_or_insert_with(AssetPanels::default);
    match rules {
        Ok(rules) => {
            let scope = panels.problems.last_scope.clone().unwrap_or(Scope::Project);
            panels.source = PanelSource::Registry;
            panels.error = None;
            panels.validate(&set, &rules, scope);
        }
        Err(err) => panels.error = Some(err),
    }
}

/// What the Reference Graph shows before an asset is chosen (bug 25, ADR-8586).
pub const REFERENCE_GRAPH_EMPTY: &str =
    "No asset chosen yet. Right-click an asset in the Content Browser and choose Reference graph.";

/// What the Size Map shows before an asset is chosen (bug 25, ADR-8586).
pub const SIZE_MAP_EMPTY: &str =
    "No asset chosen yet. Right-click an asset in the Content Browser and choose Size map.";

/// The Reference graph panel.
pub fn reference_graph_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut panels = world.remove_resource::<AssetPanels>().unwrap_or_default();
    let mut actions: Vec<GraphAction> = Vec::new();
    let mut fix: Option<String> = None;
    // Wraps in a narrow dock instead of cutting "Open size map" off (bug 19, ADR-8586).
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(panels.graph.can_back(), egui::Button::new("<"))
            .clicked()
        {
            actions.push(GraphAction::Back);
        }
        if ui
            .add_enabled(panels.graph.can_forward(), egui::Button::new(">"))
            .clicked()
        {
            actions.push(GraphAction::Forward);
        }
        let query = &panels.graph.query;
        let (mut referrers, mut dependencies, mut width) =
            (query.referrer_depth, query.dependency_depth, query.max_per_column);
        ui.label("referrers");
        if ui
            .add(egui::DragValue::new(&mut referrers).range(0..=8))
            .changed()
        {
            actions.push(GraphAction::Depth(Side::Referrers, referrers));
        }
        ui.label("dependencies");
        if ui
            .add(egui::DragValue::new(&mut dependencies).range(0..=8))
            .changed()
        {
            actions.push(GraphAction::Depth(Side::Dependencies, dependencies));
        }
        ui.label("width");
        if ui.add(egui::DragValue::new(&mut width).range(1..=500)).changed() {
            actions.push(GraphAction::Width(width));
        }
        if ui.button("Open size map").clicked() {
            actions.push(GraphAction::OpenSizeMap);
        }
    });
    // Diagnostic colors from the theme (Preferences ▸ Theme ▸ Diagnostic colors, ADR-7838).
    let theme = crate::theme::ui_tokens(ui.ctx());
    let severity_color = |severity: Severity| {
        let level = match severity {
            Severity::Error => crate::theme::DiagnosticLevel::Error,
            Severity::Warning => crate::theme::DiagnosticLevel::Warning,
            Severity::Info => crate::theme::DiagnosticLevel::Info,
        };
        crate::theme::diagnostic_color(&theme, level)
    };
    if let Some(error) = &panels.error {
        ui.colored_label(severity_color(Severity::Error), error);
    }
    if let Some(result) = &panels.graph.result {
        ui.horizontal_wrapped(|ui| {
            ui.label("types:");
            for kind in &result.types_present {
                let on = panels.graph.query.kinds.is_empty() || panels.graph.query.kinds.contains(kind);
                if ui.selectable_label(on, kind_name(*kind)).clicked() {
                    actions.push(GraphAction::Kind(*kind));
                }
            }
        });
        egui::ScrollArea::both().show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let referrers: Vec<&Column> = result
                    .columns
                    .iter()
                    .rev()
                    .filter(|c| c.side == Side::Referrers)
                    .collect();
                for column in referrers {
                    graph_column(ui, column, &mut actions);
                }
                ui.vertical(|ui| {
                    ui.strong("selection");
                    for root in &result.roots {
                        ui.label(root);
                    }
                });
                for column in result.columns.iter().filter(|c| c.side == Side::Dependencies) {
                    graph_column(ui, column, &mut actions);
                }
            });
            if !result.missing.is_empty() {
                // Each missing reference can be retargeted onto an asset (ADR-7822).
                ui.label("missing:");
                for missing in &result.missing {
                    missing_reference_row(ui, missing, &mut fix);
                }
            }
        });
    } else {
        ui.weak(REFERENCE_GRAPH_EMPTY);
    }
    if !actions.is_empty() {
        apply_graph_actions(world, &mut panels, actions);
    }
    world.insert_resource(panels);
    if let Some(missing) = fix {
        crate::content_actions::open_picker(world, crate::content_actions::PickerPurpose::Retarget, &missing);
    }
}

fn graph_column(ui: &mut egui::Ui, column: &Column, actions: &mut Vec<GraphAction>) {
    ui.vertical(|ui| {
        ui.strong(format!("{} {}", column.side.label(), column.hop));
        for node in &column.nodes {
            if ui
                .selectable_label(false, node)
                .on_hover_text("re-center here")
                .clicked()
            {
                actions.push(GraphAction::Recenter(node.clone()));
            }
        }
        if column.hidden > 0 && ui.button(format!("{} more", column.hidden)).clicked() {
            actions.push(GraphAction::More(column.side, column.hop));
        }
    });
}

fn kind_name(kind: AssetKind) -> String {
    format!("{kind:?}").to_lowercase()
}

fn apply_graph_actions(world: &World, panels: &mut AssetPanels, actions: Vec<GraphAction>) {
    let Some(set) = registry_set(world) else {
        panels.error = Some("registry_not_loaded: cook the project first".to_owned());
        return;
    };
    for action in actions {
        let result = match action {
            GraphAction::Back => panels.graph.step(&set, -1).map(|_| ()),
            GraphAction::Forward => panels.graph.step(&set, 1).map(|_| ()),
            GraphAction::Depth(side, depth) => {
                let mut query = panels.graph.query.clone();
                match side {
                    Side::Referrers => query.referrer_depth = depth,
                    Side::Dependencies => query.dependency_depth = depth,
                }
                panels.graph.show(&set, query)
            }
            GraphAction::Width(width) => {
                let mut query = panels.graph.query.clone();
                query.max_per_column = width;
                query.pages.clear();
                panels.graph.show(&set, query)
            }
            GraphAction::Kind(kind) => panels.graph.toggle_kind(&set, kind),
            GraphAction::More(side, hop) => panels.graph.more(&set, side, hop),
            GraphAction::Recenter(asset) => {
                panels.select(asset.clone());
                panels.graph.recenter(&set, &asset)
            }
            GraphAction::OpenSizeMap => panels.open_size_map(&set),
        };
        panels.error = result.err().map(|e| format!("{}: {e}", e.code()));
    }
}

/// The Size map panel: a squarified treemap of the rows.
pub fn size_map_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut panels = world.remove_resource::<AssetPanels>().unwrap_or_default();
    // Diagnostic colors from the theme (Preferences ▸ Theme ▸ Diagnostic colors, ADR-7838).
    let theme = crate::theme::ui_tokens(ui.ctx());
    let severity_color = |severity: Severity| {
        let level = match severity {
            Severity::Error => crate::theme::DiagnosticLevel::Error,
            Severity::Warning => crate::theme::DiagnosticLevel::Warning,
            Severity::Info => crate::theme::DiagnosticLevel::Info,
        };
        crate::theme::diagnostic_color(&theme, level)
    };
    if let Some(error) = &panels.error {
        ui.colored_label(severity_color(Severity::Error), error);
    }
    if let Some(map) = &panels.size_map.result {
        ui.label(format!(
            "{} assets, {} in total, {} shared, {} exclusive",
            map.closure_assets,
            format_bytes(map.total_bytes),
            format_bytes(map.shared_bytes),
            format_bytes(map.exclusive_bytes),
        ));
    } else {
        ui.weak(SIZE_MAP_EMPTY);
        world.insert_resource(panels);
        return;
    }
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), egui::Sense::hover());
    panels.size_map.layout(rect.width(), rect.height());
    let painter = ui.painter_at(rect);
    let cells = panels.size_map.cells.clone();
    for cell in &cells {
        let area =
            egui::Rect::from_min_size(rect.min + egui::vec2(cell.x, cell.y), egui::vec2(cell.w, cell.h));
        // The hue walks with the row index, so neighbors differ.
        let hue = (cell.index as f32 * 0.618_034).fract();
        let fill = egui::ecolor::Hsva::new(hue, 0.45, 0.55, 1.0);
        painter.rect_filled(area.shrink(0.5), 0.0, fill);
        if let Some(row) = panels
            .size_map
            .result
            .as_ref()
            .and_then(|m| m.rows.get(cell.index))
            && cell.w > 60.0
            && cell.h > 16.0
        {
            let name = row.name.rsplit('/').next().unwrap_or(&row.name);
            painter.with_clip_rect(area).text(
                area.min + egui::vec2(3.0, 2.0),
                egui::Align2::LEFT_TOP,
                name,
                egui::FontId::proportional(12.0),
                egui::Color32::WHITE,
            );
        }
    }
    if let Some(pointer) = response.hover_pos()
        && let Some(hover) = panels
            .size_map
            .hover(pointer.x - rect.min.x, pointer.y - rect.min.y)
    {
        response.on_hover_text(format!(
            "{}\n{} ({:.2} %)\nexclusive {}",
            hover.name,
            format_bytes(hover.bytes),
            f64::from(hover.share_bp) / 100.0,
            format_bytes(hover.exclusive_bytes),
        ));
    }
    world.insert_resource(panels);
}

/// The Problems rows of the level's configuration warnings (`crate::config_checks`, ADR-7772): one
/// warning-level row per warning, validator `config_checks`, the entity's name as the asset.
#[must_use]
pub fn config_warning_rows(world: &World) -> Vec<ProblemRow> {
    let Some(checks) = world.get_resource::<crate::config_checks::ConfigChecks>() else {
        return Vec::new();
    };
    checks
        .all()
        .iter()
        .flat_map(|(entity, warnings)| {
            let name = crate::outliner::display_name(world, *entity);
            warnings.iter().map(move |warning| ProblemRow {
                severity: Severity::Warning,
                code: warning.code.to_string(),
                validator: "config_checks".to_string(),
                asset: name.clone(),
                text: warning.text.clone(),
                hint: Some(warning.fix_hint.clone()),
            })
        })
        .collect()
}

/// The Problems panel's section of configuration warnings; a click selects the entity. The checks
/// run on change only (`crate::config_checks::update`).
fn entity_warnings_section(ui: &mut egui::Ui, world: &mut World) {
    crate::config_checks::update(world);
    let Some(checks) = world.get_resource::<crate::config_checks::ConfigChecks>() else {
        return;
    };
    if checks.all().is_empty() {
        return;
    }
    let entities: Vec<Entity> = checks.all().keys().copied().collect();
    let rows = config_warning_rows(world);
    ui.separator();
    ui.strong(format!("Level configuration ({} warning(s))", rows.len()));
    let mut picked = None;
    let mut index = 0;
    for entity in entities {
        let count = world
            .get_resource::<crate::config_checks::ConfigChecks>()
            .map_or(0, |c| c.warnings_of(entity).len());
        for row in rows.iter().skip(index).take(count) {
            let line = format!("warning {} {}: {}", row.code, row.asset, row.text);
            let response = ui.selectable_label(false, line);
            let response = match &row.hint {
                Some(hint) => response.on_hover_text(hint),
                None => response,
            };
            if response.clicked() {
                picked = Some(entity);
            }
        }
        index += count;
    }
    if let Some(entity) = picked {
        let mut selection = world
            .get_resource::<crate::selection::Selection>()
            .cloned()
            .unwrap_or_default();
        selection.select(entity, crate::selection::SelectMode::Replace);
        world.insert_resource(selection);
    }
}

/// The open material graphs' compile errors (w13, P-185): one line per error with an **Open**
/// link (a focusable button named "Open `<target>` in `<tab>`") that activates the tab, selects
/// the node and brings the Material Graph panel forward. `editor.problems_panel`'s
/// `graph_errors` and `open_graph_error` are its agent twin.
fn graph_errors_section(ui: &mut egui::Ui, world: &mut World) {
    let problems = crate::matgraph_editor::problems::world_problems(world);
    if problems.is_empty() {
        return;
    }
    let theme = crate::theme::ui_tokens(ui.ctx());
    let color = crate::theme::diagnostic_color(&theme, crate::theme::DiagnosticLevel::Error);
    ui.separator();
    ui.strong(format!("Material graphs ({} error(s))", problems.len()));
    let mut open = None;
    for (index, problem) in problems.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(problem.line()).color(color));
            let name = format!("Open {} in {}", problem.target, problem.label);
            let button = crate::a11y_audit::name(ui.small_button("Open"), egui::WidgetType::Button, &name);
            if button.clicked() {
                open = Some(index);
            }
        });
    }
    // egui may run the panel twice in a frame: follow a link once per frame.
    let frame = ui.ctx().cumulative_frame_nr();
    let guard = egui::Id::new("problems-graph-open-frame");
    if let Some(index) = open
        && ui.data(|d| d.get_temp::<u64>(guard)) != Some(frame)
    {
        ui.data_mut(|d| d.insert_temp(guard, frame));
        if let Err(err) = crate::matgraph_editor::problems::open_world_problem(world, index) {
            world.get_resource_or_insert_with(AssetPanels::default).error = Some(err);
        }
    }
}

/// Validates `paths` in one run (the Content Browser's bulk Validate, ADR-7822); the findings
/// fill the Problems panel. Answers the toast line.
pub fn validate_paths(world: &mut World, paths: &[String]) -> String {
    let Some(set) = registry_set(world) else {
        return "no asset registry is loaded: cook the project first".to_owned();
    };
    let rules = match project_rules(world) {
        Ok(rules) => rules,
        Err(err) => return err,
    };
    let mut panels = world.get_resource_or_insert_with(AssetPanels::default);
    panels.source = PanelSource::Registry;
    panels.error = None;
    panels.validate(&set, &rules, Scope::Assets(paths.to_vec()));
    problems_toast(&panels)
}

/// **Retarget…** and **Find similar** of one missing reference (both open the asset picker; Find
/// similar lists the assets with the same file name first). Focusable buttons.
fn missing_reference_buttons(ui: &mut egui::Ui, missing: &str, fix: &mut Option<String>) {
    let retarget = crate::agent_parity::action_button(ui, &crate::agent_parity::MISSING_RETARGET).clicked();
    let similar =
        crate::agent_parity::action_button(ui, &crate::agent_parity::MISSING_FIND_SIMILAR).clicked();
    if retarget || similar {
        *fix = Some(missing.to_owned());
    }
}

/// One missing reference of the Reference graph: its path and the fixer's buttons.
fn missing_reference_row(ui: &mut egui::Ui, missing: &str, fix: &mut Option<String>) {
    ui.horizontal(|ui| {
        ui.label(missing);
        missing_reference_buttons(ui, missing, fix);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SET: &str = r##"#![enable(implicit_some)]
    (
        format_version: 2,
        assets: [
            (path: "scenes/main.scene", kind: scene, references: ["meshes/rock.mesh", "meshes/tree.mesh", "textures/gone.png"], cooked_bytes: 10),
            (path: "meshes/rock.mesh", kind: mesh, references: ["textures/shared.png"], cooked_bytes: 100),
            (path: "meshes/tree.mesh", kind: mesh, references: ["textures/shared.png"], cooked_bytes: 200),
            (path: "textures/shared.png", kind: texture, cooked_bytes: 1000, texture_size: (8192, 8192)),
            (path: "textures/loose.png", kind: texture, cooked_bytes: 5),
        ],
    )"##;

    fn set() -> AssetSet {
        AssetSet::from_ron(SET).expect("set")
    }

    fn validated() -> AssetPanels {
        let mut panels = AssetPanels::default();
        panels.validate(&set(), &ValidationFile::default(), Scope::Project);
        panels
    }

    fn root(asset: &str) -> GraphQuery {
        GraphQuery {
            roots: vec![asset.to_owned()],
            ..GraphQuery::default()
        }
    }

    #[test]
    fn problems_sort_worst_first_and_filter_by_severity_and_validator() {
        let panels = validated();
        let rows = &panels.problems.rows;
        assert!(!rows.is_empty());
        let ranks: Vec<usize> = rows.iter().map(|r| severity_rank(r.severity)).collect();
        let mut sorted = ranks.clone();
        sorted.sort_unstable();
        assert_eq!(ranks, sorted, "worst first");
        assert!(rows.iter().any(|r| r.code == "validate_reference_missing"));

        let mut filtered = panels.problems.clone();
        filtered.show_severity(Severity::Error, false);
        assert!(filtered.visible().iter().all(|r| r.severity != Severity::Error));
        filtered.show_severity(Severity::Error, true);
        assert_eq!(filtered.visible().len(), rows.len());

        filtered.validator = Some("missing_references".to_owned());
        assert!(!filtered.visible().is_empty());
        assert!(
            filtered
                .visible()
                .iter()
                .all(|r| r.validator == "missing_references")
        );
        assert!(panels.problems.validators().windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn a_click_is_a_selection_event_and_nothing_else() {
        let mut panels = validated();
        let before = panels.problems.clone();
        panels.select("textures/gone.png");
        assert_eq!(panels.take_selected().as_deref(), Some("textures/gone.png"));
        assert_eq!(panels.take_selected(), None);
        assert_eq!(panels.problems, before);
    }

    #[test]
    fn rerun_repeats_the_scope() {
        let mut panels = AssetPanels::default();
        let scope = Scope::Assets(vec!["meshes/rock.mesh".to_owned()]);
        panels.validate(&set(), &ValidationFile::default(), scope.clone());
        assert_eq!(panels.problems.last_scope, Some(scope));
        let report = panels.problems.report.as_ref().expect("report");
        assert!(report.results.iter().all(|row| row.asset == "meshes/rock.mesh"));
    }

    #[test]
    fn the_graph_keeps_a_history_of_queries() {
        let set = set();
        let mut graph = ReferenceGraphState::default();
        assert!(!graph.can_back() && !graph.can_forward());
        graph.show(&set, root("meshes/rock.mesh")).expect("one");
        graph.recenter(&set, "textures/shared.png").expect("two");
        assert!(graph.can_back() && !graph.can_forward());
        assert_eq!(graph.query.roots, ["textures/shared.png"]);

        assert!(graph.step(&set, -1).expect("back"));
        assert_eq!(graph.query.roots, ["meshes/rock.mesh"]);
        assert!(graph.can_forward());
        assert!(!graph.step(&set, -1).expect("no earlier"));
        assert!(graph.step(&set, 1).expect("forward"));
        assert_eq!(graph.query.roots, ["textures/shared.png"]);

        // A new query after going back drops the forward history.
        graph.step(&set, -1).expect("back");
        graph.recenter(&set, "meshes/tree.mesh").expect("three");
        assert!(!graph.can_forward());
    }

    #[test]
    fn an_error_leaves_the_graph_as_it_was() {
        let set = set();
        let mut graph = ReferenceGraphState::default();
        graph.show(&set, root("meshes/rock.mesh")).expect("one");
        let before = graph.clone();
        let err = graph.show(&set, root("nope")).expect_err("unknown");
        assert_eq!(err.code(), "unknown_asset");
        assert_eq!(graph, before);
    }

    #[test]
    fn the_more_node_pages_a_column_and_wraps() {
        let mut assets = String::new();
        let mut uses = Vec::new();
        for n in 0..5 {
            uses.push(format!("\"t/{n}.png\""));
            assets += &format!("(path: \"t/{n}.png\", kind: texture, cooked_bytes: 1),");
        }
        let text = format!(
            "(format_version: 2, assets: [(path: \"s.scene\", kind: scene, references: [{}], cooked_bytes: 1), {assets}])",
            uses.join(",")
        );
        let set = AssetSet::from_ron(&text).expect("set");
        let mut graph = ReferenceGraphState::default();
        let mut query = root("s.scene");
        query.max_per_column = 2;
        graph.show(&set, query).expect("show");
        let first = graph.column(Side::Dependencies, 1).expect("column").clone();
        assert_eq!((first.nodes.len(), first.hidden), (2, 3));

        graph.more(&set, Side::Dependencies, 1).expect("more");
        let second = graph.column(Side::Dependencies, 1).expect("column").clone();
        assert_eq!(second.page, 1);
        assert_ne!(second.nodes, first.nodes);

        graph.more(&set, Side::Dependencies, 1).expect("more");
        graph.more(&set, Side::Dependencies, 1).expect("wraps");
        assert_eq!(graph.column(Side::Dependencies, 1).expect("column").page, 0);
    }

    #[test]
    fn the_type_filter_comes_from_types_present_and_toggles() {
        let set = set();
        let mut graph = ReferenceGraphState::default();
        graph.show(&set, root("scenes/main.scene")).expect("show");
        let present = graph.result.as_ref().expect("result").types_present.clone();
        assert!(present.contains(&AssetKind::Mesh));
        graph.toggle_kind(&set, AssetKind::Texture).expect("on");
        assert_eq!(graph.query.kinds, [AssetKind::Texture]);
        assert_eq!(graph.result.as_ref().expect("result").types_present, present);
        graph.toggle_kind(&set, AssetKind::Texture).expect("off");
        assert!(graph.query.kinds.is_empty());
    }

    #[test]
    fn the_graph_opens_the_size_map_of_its_roots() {
        let set = set();
        let mut panels = AssetPanels::default();
        panels.graph.show(&set, root("meshes/rock.mesh")).expect("show");
        panels.open_size_map(&set).expect("open");
        let map = panels.size_map.result.as_ref().expect("map");
        assert_eq!(map.total_bytes, 1100);
    }

    #[test]
    fn hover_reads_the_row_under_the_point() {
        let set = set();
        let mut state = SizeMapState::default();
        state
            .show(
                &set,
                SizeMapQuery {
                    roots: vec!["scenes/main.scene".to_owned()],
                    ..SizeMapQuery::default()
                },
            )
            .expect("show");
        state.layout(400.0, 300.0);
        assert_eq!(state.cells.len(), 4);
        let biggest = state.cells.iter().find(|c| c.index == 0).copied().expect("cell");
        let hover = state
            .hover(biggest.x + biggest.w / 2.0, biggest.y + biggest.h / 2.0)
            .expect("hover");
        assert_eq!(hover.name, "textures/shared.png");
        assert_eq!(hover.bytes, 1000);
        assert_eq!(hover.share_bp, (1000_u32 * 10_000) / 1310);
        assert_eq!(state.hover(-1.0, -1.0), None);
    }

    #[test]
    fn panels_never_touch_the_set() {
        let original = set();
        let working = original.clone();
        let mut panels = validated();
        panels
            .graph
            .show(&working, root("meshes/rock.mesh"))
            .expect("graph");
        panels.open_size_map(&working).expect("map");
        assert_eq!(working, original);
    }

    #[test]
    fn the_panels_draw_without_a_project() {
        let mut world = World::new();
        let mut panels = validated();
        panels
            .graph
            .show(&set(), root("scenes/main.scene"))
            .expect("graph");
        panels.open_size_map(&set()).expect("map");
        world.insert_resource(panels);
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            problems_panel(ui, &mut world);
            reference_graph_panel(ui, &mut world);
            size_map_panel(ui, &mut world);
        });
        output.textures_delta.clear();
        assert!(world.contains_resource::<AssetPanels>());
    }

    #[test]
    fn a_menu_action_without_a_registry_says_so() {
        let mut world = World::new();
        let toast = run_action(&mut world, AssetAction::ValidateProject);
        assert!(toast.contains("cook the project"), "{toast}");
    }

    #[test]
    fn bytes_read_in_the_right_unit() {
        assert_eq!(format_bytes(12), "12 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    fn finding(severity: Severity, validator: &str, asset: &str, text: &str) -> ProblemRow {
        ProblemRow {
            severity,
            code: format!("{validator}_code"),
            validator: validator.to_owned(),
            asset: asset.to_owned(),
            text: text.to_owned(),
            hint: None,
        }
    }

    fn grouped_state() -> ProblemsState {
        let guid = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        ProblemsState {
            rows: vec![
                finding(Severity::Error, "missing_references", "a.level", "gone.png"),
                finding(Severity::Warning, "naming_rules", "B.png", "upper case"),
                finding(Severity::Warning, "naming_rules", "B.png", "upper case"),
                finding(Severity::Warning, "naming_rules", "B.png", "upper case"),
                finding(
                    Severity::Warning,
                    "missing_references",
                    "c.level",
                    &format!("entity {guid} has no mesh"),
                ),
                finding(Severity::Info, "naming_rules", "d.png", "long"),
            ],
            ..ProblemsState::default()
        }
    }

    /// P-154: identical consecutive findings show once with a count; grouping lists one header per
    /// source, worst source first; a closed group keeps only its header; Copy all follows the list.
    #[test]
    fn problems_group_by_source_and_collapse_duplicates() {
        let mut problems = grouped_state();
        let runs = problems.visible_runs();
        assert_eq!(runs.len(), 4);
        assert_eq!((runs[1].1, runs[1].2), (1, 3), "rows 1..=3 are one run of 3");
        problems.collapse_duplicates = false;
        assert_eq!(problems.visible_runs().len(), 6);
        problems.collapse_duplicates = true;

        problems.grouped = true;
        let items = problems.items();
        assert_eq!(items.len(), 2 + 4);
        assert!(
            matches!(
                items[0],
                ProblemItem::Header {
                    source: "missing_references",
                    rows: 2,
                    worst: Severity::Error,
                    open: true
                }
            ),
            "{items:?}"
        );
        assert!(matches!(
            items[3],
            ProblemItem::Header {
                source: "naming_rules",
                rows: 2,
                worst: Severity::Warning,
                ..
            }
        ));
        problems.toggle_group("missing_references");
        let items = problems.items();
        assert_eq!(items.len(), 1 + 1 + 2);
        assert_eq!(
            problems.copy_all_text(),
            "[missing_references] (2)\n[naming_rules] (2)\nwarning naming_rules_code B.png: upper case (×3)\ninfo naming_rules_code d.png: long"
        );
        // The severity filter applies before the grouping.
        problems.hidden_severities = vec![Severity::Info, Severity::Warning];
        assert_eq!(
            problems.items().len(),
            1,
            "only the closed missing_references header"
        );

        assert_eq!(problems.selected_text(), None);
        problems.selected_row = Some(4);
        assert!(
            problems
                .selected_text()
                .is_some_and(|t| t.starts_with("warning missing_references_code c.level"))
        );
        assert_eq!(
            problem_entities(&problems.rows[4]),
            ["67e55044-10b1-426f-9247-bb680e5fe0c8"]
        );
    }

    /// Review sprint-w12-rev-g5 #6: egui's second UI pass of a frame sees the same click; the
    /// Problems controls apply it once, so grouping, a severity and a group header stick.
    #[test]
    fn problems_toggles_apply_once_per_frame() {
        let mut problems = grouped_state();
        assert!(problems.apply_toggles(vec![ProblemToggle::Grouped(true)], 5));
        // The second pass reads `grouped: true` and its checkbox asks for false.
        assert!(!problems.apply_toggles(vec![ProblemToggle::Grouped(false)], 5));
        assert!(problems.grouped, "grouping sticks");
        assert!(problems.apply_toggles(
            vec![
                ProblemToggle::Group("naming_rules".into()),
                ProblemToggle::Severity(Severity::Info, false),
                ProblemToggle::CollapseDuplicates(false),
            ],
            6
        ));
        assert!(!problems.apply_toggles(vec![ProblemToggle::Group("naming_rules".into())], 6));
        assert!(
            problems.closed_groups.contains("naming_rules"),
            "the header stays collapsed"
        );
        assert!(problems.hidden_severities.contains(&Severity::Info));
        assert!(!problems.collapse_duplicates);
        assert!(
            !problems.apply_toggles(Vec::new(), 7),
            "nothing asked: nothing applied"
        );
        assert!(problems.apply_toggles(vec![ProblemToggle::Grouped(false)], 7));
        assert!(!problems.grouped);
    }

    /// Accessibility bar: Group by source and a group header work from the keyboard.
    #[test]
    fn problems_grouping_works_from_the_keyboard() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let mut world = World::new();
        world.insert_resource(AssetPanels {
            problems: grouped_state(),
            ..AssetPanels::default()
        });
        let ctx = egui::Context::default();
        let size = [1400.0, 700.0];
        let draw = |ui: &mut egui::Ui, world: &mut World| {
            egui::CentralPanel::default().show(ui, |ui| problems_panel(ui, world));
        };
        frame(&ctx, size, vec![], |ui| draw(ui, &mut world));
        tab_to(&ctx, size, "Group by source", false, 40, |ui| {
            draw(ui, &mut world)
        });
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Space, egui::Modifiers::NONE)],
            |ui| draw(ui, &mut world),
        );
        assert!(world.resource::<AssetPanels>().problems.grouped);
        tab_to(&ctx, size, "▼ naming_rules (2)", false, 40, |ui| {
            draw(ui, &mut world)
        });
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            |ui| draw(ui, &mut world),
        );
        assert!(
            world
                .resource::<AssetPanels>()
                .problems
                .closed_groups
                .contains("naming_rules")
        );
        tab_to(&ctx, size, "Copy all", false, 40, |ui| draw(ui, &mut world));
    }
}
