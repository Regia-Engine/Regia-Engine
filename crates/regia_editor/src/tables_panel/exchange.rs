// SPDX-License-Identifier: MPL-2.0
//! Import and export of the Tables panel (`book/agents/editor-data-panels.md`, parity row P-282): a data
//! table or a curve table as CSV or JSON, and a data table as canonical `.table.ron`, with the same
//! library calls as `regia table to-csv|to-ron|to-json|from-json`.
//!
//! - **Export** writes the open table of the shown tab to a file, atomically. A data table goes out as CSV
//!   (`regia table to-csv`), JSON (an array of row objects keyed by `key`, `regia table to-json`) or the
//!   canonical `.table.ron` (`regia table to-ron`); a curve table as CSV or JSON in the layout of its CSV
//!   form (a curve per row, an x per column). Exporting never marks the table saved.
//! - **Import** replaces the open table with the file's rows, as one undo step. A data table keeps its row
//!   type and its text columns (the cell rules of `regia table to-ron --into`); an import that would add a
//!   schema error is refused and changes nothing. A curve table is replaced through the same operation as
//!   the CSV paste box.
//!
//! A chooser has no tabular form and refuses both.

use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::data::SchemaLookup;
use super::{PREFIX, TablesPanel, TablesTab, err, tab_param, with_panel, with_panel_lookup};
use crate::data_panels::{PanelError, str_param, write_atomic};
use crate::viewport_agent::Handler;

/// The file form of an import or export.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExchangeFormat {
    /// RFC 4180 CSV (`.csv`).
    #[default]
    Csv,
    /// A JSON array of row objects keyed by `key` (`.json`).
    Json,
    /// The canonical `.table.ron` (data tables only).
    Ron,
}

impl ExchangeFormat {
    /// Every format, in menu order.
    pub const ALL: [ExchangeFormat; 3] = [Self::Csv, Self::Json, Self::Ron];

    /// `csv`, `json` or `ron`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
            Self::Ron => "ron",
        }
    }

    /// The format named `name`.
    ///
    /// # Errors
    ///
    /// `tables_panel_bad_format` for an unknown name.
    pub fn parse(name: &str) -> Result<Self, PanelError> {
        Self::ALL
            .into_iter()
            .find(|f| f.name() == name)
            .ok_or_else(|| err("bad_format", format!("unknown format `{name}` (csv, json, ron)")))
    }

    /// The format a file name suggests (`.csv`, `.json`, `.ron`), or `None`.
    #[must_use]
    pub fn of_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "csv" => Some(Self::Csv),
            "json" => Some(Self::Json),
            "ron" => Some(Self::Ron),
            _ => None,
        }
    }
}

/// What an import or export did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExchangeOutcome {
    /// The tab whose table was read or written.
    pub tab: TablesTab,
    /// The file form.
    pub format: ExchangeFormat,
    /// The file.
    pub file: PathBuf,
    /// Rows written (data table) or curves written (curve table); for an import, the rows read.
    pub rows: usize,
    /// Whether this was an import.
    pub imported: bool,
}

impl TablesPanel {
    fn exchange_format(format: Option<ExchangeFormat>, file: &Path) -> Result<ExchangeFormat, PanelError> {
        format.or_else(|| ExchangeFormat::of_path(file)).ok_or_else(|| {
            err(
                "bad_format",
                format!(
                    "{}: name a format (csv, json, ron) or use a .csv, .json or .ron file",
                    file.display()
                ),
            )
        })
    }

    /// Writes the table of `tab` (the shown tab by default) to `file` as `format` (default: from the file's
    /// extension), atomically.
    ///
    /// # Errors
    ///
    /// `tables_panel_no_table`, `tables_panel_not_exchangeable` (a chooser, or a curve table as `ron`),
    /// `tables_panel_bad_format`, `tables_panel_io_failed`, or a writer's code.
    pub fn export_table(
        &mut self,
        tab: Option<TablesTab>,
        format: Option<ExchangeFormat>,
        file: &Path,
    ) -> Result<ExchangeOutcome, PanelError> {
        let tab = tab.unwrap_or(self.tab);
        let format = Self::exchange_format(format, file)?;
        let (text, rows) = match tab {
            TablesTab::Data => {
                let doc = self
                    .data
                    .as_ref()
                    .ok_or_else(|| err("no_table", "open a data table first"))?;
                (doc.export_text(format)?, doc.raw.rows.len())
            }
            TablesTab::Curves => {
                let doc = self.curve_doc()?;
                let csv = doc
                    .table
                    .to_csv_string()
                    .map_err(|e| PanelError::new(e.code(), e.to_string()))?;
                let text = match format {
                    ExchangeFormat::Csv => csv,
                    ExchangeFormat::Json => regia_asset_pipeline::table_json::csv_to_json(&csv)
                        .map_err(|issue| PanelError::new(issue.code.code(), issue.to_string()))?,
                    ExchangeFormat::Ron => {
                        return Err(err(
                            "not_exchangeable",
                            "a curve table exports as csv or json; Save writes its .curves.ron",
                        ));
                    }
                };
                (text, doc.table.curves().len())
            }
            TablesTab::Chooser => {
                return Err(err("not_exchangeable", "a chooser has no CSV or JSON form"));
            }
        };
        write_atomic(file, &text, PREFIX)?;
        Ok(ExchangeOutcome {
            tab,
            format,
            file: file.to_path_buf(),
            rows,
            imported: false,
        })
    }

    /// Replaces the table of `tab` (the shown tab by default) with the rows of `file`, as `format` (default:
    /// from the file's extension), as one undo step; the table is then unsaved.
    ///
    /// # Errors
    ///
    /// `tables_panel_no_table`, `tables_panel_not_exchangeable`, `tables_panel_bad_format`,
    /// `tables_panel_file_not_found`, the reader's `table_*` or `curve_*` code, or the first schema error an
    /// import would add (nothing is changed then).
    pub(super) fn import_table(
        &mut self,
        tab: Option<TablesTab>,
        format: Option<ExchangeFormat>,
        file: &Path,
        lookup: &SchemaLookup<'_>,
    ) -> Result<ExchangeOutcome, PanelError> {
        let tab = tab.unwrap_or(self.tab);
        let format = Self::exchange_format(format, file)?;
        let text = std::fs::read_to_string(file)
            .map_err(|e| err("file_not_found", format!("{}: {e}", file.display())))?;
        let rows = match tab {
            TablesTab::Data => {
                let doc = self
                    .data
                    .as_mut()
                    .ok_or_else(|| err("no_table", "open a data table first"))?;
                doc.import_text(&text, format, lookup)?
            }
            TablesTab::Curves => {
                let csv = match format {
                    ExchangeFormat::Csv => text,
                    ExchangeFormat::Json => {
                        regia_asset_pipeline::table_json::json_to_csv_keyed(&text, "name")
                            .map_err(|issue| PanelError::new(issue.code.code(), issue.to_string()))?
                    }
                    ExchangeFormat::Ron => {
                        return Err(err(
                            "not_exchangeable",
                            "a curve table imports from csv or json; open the .curves.ron to edit it",
                        ));
                    }
                };
                self.curve_edit(&json!({ "op": "import_csv", "csv": csv }))?;
                self.curve_doc()?.table.curves().len()
            }
            TablesTab::Chooser => {
                return Err(err("not_exchangeable", "a chooser has no CSV or JSON form"));
            }
        };
        Ok(ExchangeOutcome {
            tab,
            format,
            file: file.to_path_buf(),
            rows,
            imported: true,
        })
    }
}

/// The Import and Export row of the panel: a format picker, a file field with a picker, and the buttons.
/// Returns the outcome or the error of the button clicked.
pub(super) fn draw_exchange(
    ui: &mut egui::Ui,
    panel: &mut TablesPanel,
    project: Option<&Path>,
    lookup: &SchemaLookup<'_>,
) -> Option<Result<ExchangeOutcome, PanelError>> {
    if panel.tab == TablesTab::Chooser {
        return None;
    }
    let open = match panel.tab {
        TablesTab::Data => panel.data.is_some(),
        TablesTab::Curves => panel.curves.is_some(),
        TablesTab::Chooser => false,
    };
    let mut result = None;
    ui.horizontal_wrapped(|ui| {
        ui.label("Exchange");
        egui::ComboBox::from_id_salt("tables_exchange_format")
            .selected_text(panel.exchange_format.name())
            .show_ui(ui, |ui| {
                for f in ExchangeFormat::ALL {
                    if f == ExchangeFormat::Ron && panel.tab != TablesTab::Data {
                        continue;
                    }
                    ui.selectable_value(&mut panel.exchange_format, f, f.name());
                }
            })
            .response
            .on_hover_text("The file form of Import and Export");
        ui.add(
            egui::TextEdit::singleline(&mut panel.exchange_path)
                .hint_text("file to import or export")
                .desired_width(200.0),
        );
        if ui
            .button("…")
            .on_hover_text("Pick the file to import or export")
            .clicked()
            && let Some(path) = crate::file_dialog::FileDialog::new()
                .add_filter("CSV / JSON / RON", &["csv", "json", "ron"])
                .pick_file()
        {
            panel.exchange_path = path.display().to_string();
            if let Some(f) = ExchangeFormat::of_path(&path)
                && (f != ExchangeFormat::Ron || panel.tab == TablesTab::Data)
            {
                panel.exchange_format = f;
            }
        }
        let file = {
            let text = panel.exchange_path.trim();
            (!text.is_empty()).then(|| {
                let path = PathBuf::from(text);
                match project {
                    Some(project) if path.is_relative() => project.join("assets").join(path),
                    _ => path,
                }
            })
        };
        let format = Some(panel.exchange_format);
        let import = ui
            .add_enabled(open && file.is_some(), egui::Button::new("Import…"))
            .on_hover_text("Replace the open table with the rows of the file (one undo step)");
        let export = ui
            .add_enabled(open && file.is_some(), egui::Button::new("Export…"))
            .on_hover_text("Write the open table to the file (regia table to-csv, to-json, to-ron)");
        #[cfg(test)]
        ui.data_mut(|d| {
            d.insert_temp(egui::Id::new("tables-exchange-test-import"), import.id);
            d.insert_temp(egui::Id::new("tables-exchange-test-export"), export.id);
        });
        if let Some(file) = &file {
            if import.clicked() {
                result = Some(panel.import_table(None, format, file, lookup));
            }
            if export.clicked() {
                result = Some(panel.export_table(None, format, file));
            }
        }
    });
    if let Some(Ok(outcome)) = &result {
        panel.last_exchange = Some(outcome.clone());
    }
    if let Some(done) = &panel.last_exchange {
        ui.weak(format!(
            "{} {} row(s) {} {}",
            if done.imported { "imported" } else { "exported" },
            done.rows,
            if done.imported { "from" } else { "to" },
            done.file.display()
        ));
    }
    result
}

// --------------------------------------------------------------------------------------- agent surface

fn outcome_json(world: &World, outcome: &ExchangeOutcome) -> Value {
    let undo_depth = world
        .get_resource::<TablesPanel>()
        .map_or(0, |p| match outcome.tab {
            TablesTab::Data => p.data.as_ref().map_or(0, |d| d.undo_depth()),
            TablesTab::Curves => p.curves.as_ref().map_or(0, |d| d.undo.len()),
            TablesTab::Chooser => 0,
        });
    json!({
        "tab": outcome.tab.name(),
        "format": outcome.format.name(),
        "file": outcome.file.display().to_string(),
        "rows": outcome.rows,
        "imported": outcome.imported,
        "undo_depth": undo_depth,
    })
}

fn format_param(params: &Value) -> Result<Option<ExchangeFormat>, RpcError> {
    match params.get("format") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) => ExchangeFormat::parse(name)
            .map(Some)
            .map_err(|e| e.rpc(super::HINT)),
        Some(_) => Err(RpcError::invalid_params("`format` must be a string")),
    }
}

/// A request path under the project's `assets/` (an `assets/` prefix is dropped), like every other
/// `editor.tables_panel_*` path.
fn request_file(world: &World, text: &str) -> Result<PathBuf, RpcError> {
    let root = crate::mesh_source::project_root(world)
        .ok_or_else(|| err("no_project", "open a project first").rpc(super::HINT))?;
    crate::agent_ext::assets_request_path(&root, "path", text)
}

/// `editor.tables_panel_import`.
fn panel_import(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let tab = tab_param(params)?;
    let format = format_param(params)?;
    let text = str_param(params, "path")?.to_owned();
    let file = request_file(world, &text)?;
    let outcome = with_panel_lookup(world, |panel, _, lookup| {
        panel.import_table(tab, format, &file, lookup)
    })?;
    Ok(outcome_json(world, &outcome))
}

/// `editor.tables_panel_export`.
fn panel_export(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let tab = tab_param(params)?;
    let format = format_param(params)?;
    let text = str_param(params, "path")?.to_owned();
    let file = request_file(world, &text)?;
    let outcome = with_panel(world, |panel, _| panel.export_table(tab, format, &file))?;
    Ok(outcome_json(world, &outcome))
}

const EXCHANGE_PARAMS: &[ParamSchema] = &[
    ParamSchema {
        name: "path",
        kind: "string",
        required: true,
        description: "The file under the project's assets/ (import: it must exist).",
    },
    ParamSchema {
        name: "format",
        kind: "string",
        required: false,
        description: "csv | json | ron (ron: data tables only); default from the file's extension.",
    },
    ParamSchema {
        name: "tab",
        kind: "string",
        required: false,
        description: "curves | data; default the shown tab.",
    },
];

const RESULT: &str = "{ tab, format, file, rows, imported, undo_depth }";

/// The import and export methods (appended to [`super::methods`]).
pub(super) fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.tables_panel_import",
                tool: "tables_panel_import",
                description: "Replace the open data table or curve table with the rows of a CSV or JSON file (a data \
                    table also from `.table.ron`), as one undo step; the table keeps its row type and text columns. An \
                    import that would add a schema error is refused and changes nothing. Save writes the table's own file.",
                params: EXCHANGE_PARAMS,
                result: RESULT,
                mutating: true,
                permission: Permission::Assets,
            },
            panel_import as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tables_panel_export",
                tool: "tables_panel_export",
                description: "Write the open data table or curve table to a CSV or JSON file (a data table also as \
                    canonical `.table.ron`), atomically, with the same library call as `regia table to-csv`, `to-json` \
                    and `to-ron`. The table is not marked saved.",
                params: EXCHANGE_PARAMS,
                result: RESULT,
                mutating: true,
                permission: Permission::Assets,
            },
            panel_export as Handler,
        ),
    ]
}
