// SPDX-License-Identifier: MPL-2.0
//! The Bulk Edit panel: a spreadsheet of many objects by one type's fields (parity row P-112,
//! spec addendum § 7.13, ADR-7787).
//!
//! Rows are the chosen entities (or `.data.ron` assets of one type); columns are the reflected
//! fields of one component type (or of the asset type), nested structs flattened to dotted paths
//! (`offset.x`). Each cell draws the Inspector's widget for its field, so a cell edits like the
//! Inspector does. The grid sorts by a column, filters rows by a text query, pins columns to the
//! left, resizes columns, copies and pastes cell ranges as tab-separated text (what a spreadsheet
//! puts on the clipboard, so a block round-trips) and fills down.
//!
//! Every commit is one undo step for the whole range on the shared [`UndoStack`]: a group of the
//! same [`SetProperty`] commands the Inspector pushes (entity rows), or one asset edit holding
//! each cell's value before and after (asset rows; written by Save). A paste or fill is checked
//! cell by cell before anything is written: one value that does not fit refuses the whole range.
//!
//! The rows are laid out only while in view (`ScrollArea::show_rows`); [`BulkEdit::drawn_rows`]
//! counts the rows the last frame laid out. Keyboard: with the grid focused (Tab to it, or click
//! a row label), the arrows move the cell cursor (Shift extends the range), Tab and Shift+Tab move
//! to the next and previous cell, Enter or F2 edits the cell as text (Enter commits, Escape
//! cancels), Ctrl+C and Ctrl+V copy and paste the range, Ctrl+D fills down, Alt+Left and
//! Alt+Right narrow and widen the cursor's column, and Escape leaves the grid.
//!
//! Agent methods (`book/agents/bulk-edit.md`): `editor.bulk_edit_open`, `editor.bulk_edit_set`,
//! `editor.bulk_edit_state`, `editor.bulk_edit_view` and `editor.bulk_edit_save`.

use std::collections::BTreeSet;
use std::path::Path;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_ecs::reflect::serde::{TypedReflectDeserializer, TypedReflectSerializer};
use regia_ecs::reflect::{
    AppTypeRegistry, PartialReflect, ReflectComponent, ReflectPath, ReflectRef, TypeRegistry,
};
use serde::de::DeserializeSeed as _;
use serde_json::{Value, json};

use crate::command::{CommandGroup, EditorCommand, SetProperty, UndoStack};
use crate::data_asset::{DATA_ASSET_EXTENSION, DataAssetDoc, DataAssetFile};
use crate::data_panels::PanelError;
use crate::inspector::{InspectorContext, InspectorUi};
use crate::viewport_agent::Handler;

/// The hint agent errors carry.
const HINT: &str = "see book/agents/bulk-edit.md";
/// How deep nested structs are flattened into columns.
pub const MAX_COLUMN_DEPTH: usize = 4;
/// A new column's width, points (scaled with the UI like every other size).
pub const DEFAULT_WIDTH: f32 = 96.0;
/// The narrowest a column gets.
pub const MIN_WIDTH: f32 = 36.0;
/// The widest a column gets.
pub const MAX_WIDTH: f32 = 640.0;
/// How much Alt+Left / Alt+Right changes the cursor's column width.
pub const WIDTH_STEP: f32 = 8.0;
/// The row label column's width.
const LABEL_WIDTH: f32 = 140.0;
/// Rows `editor.bulk_edit_state` returns when `limit` is not given.
pub const DEFAULT_LIMIT: usize = 100;
/// Type paths drawn as one cell even though they reflect as structs.
const LEAF_STRUCTS: &[&str] = &["glam::Quat", "glam::DQuat"];

/// Where the rows come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowSource {
    /// Entities of the open level; columns are one component's fields.
    Entities,
    /// `.data.ron` assets of one type; columns are that type's fields.
    Assets,
}

impl RowSource {
    /// `entities` or `assets`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Entities => "entities",
            Self::Assets => "assets",
        }
    }
}

/// One row: an entity (as bits) or an asset (project-relative path, `assets/...`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowKey {
    /// An entity, as [`Entity::to_bits`].
    Entity(u64),
    /// A data asset path.
    Asset(String),
}

impl RowKey {
    /// The key as the agent writes it: entity bits as a number, an asset as its path.
    #[must_use]
    pub fn json(&self) -> Value {
        match self {
            Self::Entity(bits) => json!(bits),
            Self::Asset(path) => json!(path),
        }
    }
}

/// One shown column.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    /// The field's reflect path inside the component (`""` is the whole component).
    pub path: String,
    /// Width in points.
    pub width: f32,
    /// Drawn left of the scrolling columns, always in view.
    pub pinned: bool,
}

/// A rectangle of cells in view coordinates (rows after sort and filter; columns in shown order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellRange {
    /// Top row.
    pub row: usize,
    /// Left column.
    pub column: usize,
    /// Row count (at least 1).
    pub rows: usize,
    /// Column count (at least 1).
    pub columns: usize,
}

impl CellRange {
    /// The range spanned by two corners.
    #[must_use]
    pub fn spanning(a: (usize, usize), b: (usize, usize)) -> Self {
        Self {
            row: a.0.min(b.0),
            column: a.1.min(b.1),
            rows: a.0.abs_diff(b.0) + 1,
            columns: a.1.abs_diff(b.1) + 1,
        }
    }

    /// Whether `(row, column)` is inside.
    #[must_use]
    pub fn contains(&self, row: usize, column: usize) -> bool {
        (self.row..self.row + self.rows).contains(&row)
            && (self.column..self.column + self.columns).contains(&column)
    }

    fn json(&self) -> Value {
        json!({ "row": self.row, "column": self.column, "rows": self.rows, "columns": self.columns })
    }
}

/// What the view order depends on: filter, sort, row count, undo state id, shown columns.
type ViewKey = (String, Option<(String, bool)>, usize, u64, Vec<String>);

/// The view order cache: rebuilt when the query, sort, rows, columns or the undo state change.
#[derive(Default)]
struct ViewCache {
    key: Option<ViewKey>,
    order: Vec<usize>,
}

/// An open grid.
pub struct Grid {
    /// Where the rows come from.
    pub source: RowSource,
    /// The component (or asset) type path.
    pub component: String,
    /// The rows, in the order they were opened.
    pub rows: Vec<RowKey>,
    /// Asset rows' documents (same order as `rows`; empty for entities).
    docs: Vec<DataAssetDoc>,
    /// Every column the type offers (flattened leaf paths).
    pub available: Vec<String>,
    /// The shown columns, in the order chosen (pinned ones are drawn first, see [`Grid::shown`]).
    pub columns: Vec<Column>,
    /// Sort column path and descending flag.
    pub sort: Option<(String, bool)>,
    /// Row filter (case-insensitive, matched against the row label and the shown cells).
    pub filter: String,
    /// The cell cursor, view coordinates.
    pub cursor: (usize, usize),
    /// The other corner of the selected range (`None`: the range is the cursor cell).
    pub anchor: Option<(usize, usize)>,
    /// The first scrolling (unpinned) column in view.
    pub first_column: usize,
    view: ViewCache,
}

impl std::fmt::Debug for Grid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grid")
            .field("source", &self.source)
            .field("component", &self.component)
            .field("rows", &self.rows.len())
            .field("columns", &self.columns)
            .finish_non_exhaustive()
    }
}

impl Grid {
    /// The shown columns in draw order: pinned first, each group in the chosen order.
    #[must_use]
    pub fn shown(&self) -> Vec<&Column> {
        let pinned = self.columns.iter().filter(|c| c.pinned);
        pinned.chain(self.columns.iter().filter(|c| !c.pinned)).collect()
    }

    /// The selected range (the cursor cell when there is no anchor).
    #[must_use]
    pub fn range(&self) -> CellRange {
        CellRange::spanning(self.cursor, self.anchor.unwrap_or(self.cursor))
    }
}

/// The cell open for text editing (Enter on the cursor cell).
#[derive(Clone, Debug, PartialEq, Eq)]
struct CellEdit {
    row: usize,
    column: usize,
    text: String,
    focused: bool,
}

/// The Bulk Edit panel's state (an edit-world resource).
#[derive(Resource, Default)]
pub struct BulkEdit {
    /// The open grid.
    pub grid: Option<Grid>,
    /// The status line (last action or error).
    pub status: String,
    /// The column picker's search text.
    pub picker_search: String,
    /// Rows the last drawn frame laid out (the virtualization counter).
    pub drawn_rows: usize,
    edit: Option<CellEdit>,
    /// The egui frame whose keys were handled (egui may run the panel twice in a frame).
    keys_frame: Option<u64>,
}

impl std::fmt::Debug for BulkEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BulkEdit")
            .field("grid", &self.grid)
            .field("status", &self.status)
            .field("drawn_rows", &self.drawn_rows)
            .finish_non_exhaustive()
    }
}

fn err(code: &str, message: impl Into<String>) -> PanelError {
    PanelError::new(code, message)
}

fn registry_of(world: &World) -> Result<AppTypeRegistry, PanelError> {
    world
        .get_resource::<AppTypeRegistry>()
        .cloned()
        .ok_or_else(|| err("bulk_edit_no_registry", "the type registry is not available"))
}

/// The registered component type `name` (full or short type path) names.
fn component_type_path(registry: &TypeRegistry, name: &str) -> Option<String> {
    registry
        .get_with_type_path(name)
        .or_else(|| registry.get_with_short_type_path(name))
        .or_else(|| regia_core::brand::lookup_type_path(name, |p| registry.get_with_type_path(p)))
        .filter(|r| r.data::<ReflectComponent>().is_some())
        .map(|r| r.type_info().type_path().to_owned())
}

/// The leaf field paths of `value`: struct fields flattened (up to [`MAX_COLUMN_DEPTH`] deep),
/// anything else one column. A value that is not a struct is the single column `""`.
#[must_use]
pub fn leaf_paths(value: &dyn PartialReflect) -> Vec<String> {
    let mut out = Vec::new();
    collect_leaves(value, "", 0, &mut out);
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn collect_leaves(value: &dyn PartialReflect, prefix: &str, depth: usize, out: &mut Vec<String>) {
    let leaf_struct = value
        .get_represented_type_info()
        .is_some_and(|info| LEAF_STRUCTS.contains(&info.type_path()));
    if let ReflectRef::Struct(fields) = value.reflect_ref()
        && depth < MAX_COLUMN_DEPTH
        && !leaf_struct
        && fields.field_len() > 0
    {
        for index in 0..fields.field_len() {
            let (Some(name), Some(field)) = (fields.name_at(index), fields.field_at(index)) else {
                continue;
            };
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}.{name}")
            };
            collect_leaves(field, &path, depth + 1, out);
        }
    } else if !prefix.is_empty() {
        out.push(prefix.to_owned());
    }
}

/// The value at `path` inside `value` (`""` is `value` itself).
fn field_at<'a>(value: &'a dyn PartialReflect, path: &str) -> Option<&'a dyn PartialReflect> {
    if path.is_empty() {
        Some(value)
    } else {
        path.reflect_element(value).ok()
    }
}

/// A cell's text: a string field is its text, anything else the reflect serializer's compact
/// JSON (`1.5`, `true`, `"Calm"` is written `Calm`, `[1.0,0.0,0.0]`).
#[must_use]
pub fn cell_text(registry: &TypeRegistry, value: &dyn PartialReflect) -> String {
    let serializer = TypedReflectSerializer::new(value, registry);
    match serde_json::to_value(&serializer) {
        Ok(Value::String(text)) => text,
        Ok(other) => other.to_string(),
        Err(_) => String::from("?"),
    }
}

/// What a write puts into a cell.
pub enum CellInput {
    /// The reflect serializer's JSON form.
    Json(Value),
    /// Cell text as [`cell_text`] writes it (or as typed in a spreadsheet): read as JSON first,
    /// then as a plain string.
    Text(String),
    /// A value of the field's type (fill down).
    Value(Box<dyn PartialReflect>),
}

/// `input` as a value of `current`'s type, or why it does not fit.
fn parse_cell(
    registry: &TypeRegistry,
    current: &dyn PartialReflect,
    input: &CellInput,
) -> Result<Box<dyn PartialReflect>, String> {
    let mut next = crate::inspector::reflect_ui::clone_concrete(registry, current)
        .ok_or_else(|| "the field cannot be copied".to_owned())?;
    let from_json = |json: &Value| -> Result<Box<dyn PartialReflect>, String> {
        let registration = current
            .get_represented_type_info()
            .and_then(|info| registry.get(info.type_id()))
            .ok_or_else(|| "the field's type is not registered".to_owned())?;
        TypedReflectDeserializer::new(registration, registry)
            .deserialize(json)
            .map_err(|e| e.to_string())
    };
    let parsed: Box<dyn PartialReflect> = match input {
        CellInput::Json(json) => from_json(json)?,
        // A string field takes the text as it is (text that reads as JSON, such as `"x"` or
        // `12`, is still the string).
        CellInput::Text(text) if current.try_downcast_ref::<String>().is_some() => Box::new(text.clone()),
        CellInput::Text(text) => {
            let trimmed = text.trim();
            let as_json = serde_json::from_str::<Value>(trimmed)
                .map_err(|e| e.to_string())
                .and_then(|json| from_json(&json));
            match as_json {
                Ok(value) => value,
                // A string field (or an enum variant name) written without quotes.
                Err(first) => from_json(&Value::String(text.clone())).map_err(|_| first)?,
            }
        }
        CellInput::Value(value) => crate::inspector::reflect_ui::clone_value(value.as_ref())
            .ok_or_else(|| "the value cannot be copied".to_owned())?,
    };
    next.try_apply(parsed.as_ref()).map_err(|e| e.to_string())?;
    Ok(next)
}

/// Splits tab-separated text into rows of cells. A cell in double quotes may hold tabs, line
/// breaks and doubled quotes (`""`), as spreadsheets write them; a trailing line break ends the
/// last row and `\r\n` counts as one break.
#[must_use]
pub fn parse_tsv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    let mut at_cell_start = true;
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cell.push('"');
                } else {
                    quoted = false;
                }
            } else {
                cell.push(c);
            }
            continue;
        }
        match c {
            '"' if at_cell_start => {
                quoted = true;
                at_cell_start = false;
            }
            '\t' => {
                row.push(std::mem::take(&mut cell));
                at_cell_start = true;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
                at_cell_start = true;
            }
            _ => {
                cell.push(c);
                at_cell_start = false;
            }
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

/// Writes rows of cells as tab-separated text, one line per row (each ending in `\n`); a cell
/// holding a tab, a line break or a double quote is quoted.
#[must_use]
pub fn write_tsv(rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index > 0 {
                out.push('\t');
            }
            if cell.contains(['\t', '\n', '\r', '"']) {
                out.push('"');
                out.push_str(&cell.replace('"', "\"\""));
                out.push('"');
            } else {
                out.push_str(cell);
            }
        }
        out.push('\n');
    }
    out
}

/// The undo stack's state id (the view cache follows it).
fn undo_state(world: &World) -> u64 {
    world.get_resource::<UndoStack>().map_or(0, UndoStack::state_id)
}

/// A row's label: an entity's name (or `#index`), an asset's file name.
fn row_label(world: &World, key: &RowKey) -> String {
    match key {
        RowKey::Entity(bits) => match crate::command::resolve_entity(world, *bits) {
            Some(entity) => world
                .get::<regia_scene::prelude::Name>(entity)
                .map(|name| name.as_str().to_owned())
                .unwrap_or_else(|| format!("#{entity}")),
            None => format!("(gone {bits})"),
        },
        RowKey::Asset(path) => path.rsplit('/').next().unwrap_or(path).to_owned(),
    }
}

/// The value of one cell (`None` when the row lacks the component or the entity is gone).
fn read_cell(world: &World, grid: &Grid, data_row: usize, path: &str) -> Option<Box<dyn PartialReflect>> {
    match grid.rows.get(data_row)? {
        RowKey::Entity(bits) => {
            let entity = crate::command::resolve_entity(world, *bits)?;
            crate::command::read_property(world, entity, &grid.component, path).ok()
        }
        RowKey::Asset(_) => {
            let doc = grid.docs.get(data_row)?;
            field_at(doc.value().as_partial_reflect(), path)
                .and_then(crate::inspector::reflect_ui::clone_value)
        }
    }
}

fn read_cell_text(
    world: &World,
    registry: &TypeRegistry,
    grid: &Grid,
    data_row: usize,
    path: &str,
) -> Option<String> {
    read_cell(world, grid, data_row, path).map(|value| cell_text(registry, value.as_ref()))
}

/// Orders two cell texts: numbers by value, otherwise by text; an empty cell last.
fn compare_cells(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => match (a.parse::<f64>(), b.parse::<f64>()) {
            (Ok(x), Ok(y)) => x.total_cmp(&y),
            _ => a.cmp(b),
        },
    }
}

/// Recomputes the view order of `grid` when its inputs changed. Returns the data row of each view
/// row.
fn refresh_view(world: &World, registry: &TypeRegistry, grid: &mut Grid) {
    let shown: Vec<String> = grid.shown().iter().map(|c| c.path.clone()).collect();
    let key = (
        grid.filter.clone(),
        grid.sort.clone(),
        grid.rows.len(),
        undo_state(world),
        shown.clone(),
    );
    if grid.view.key.as_ref() == Some(&key) {
        return;
    }
    let _span =
        tracing::info_span!(target: "regia_editor", "bulk_edit_view", rows = grid.rows.len()).entered();
    let query = grid.filter.trim().to_lowercase();
    let mut order: Vec<usize> = (0..grid.rows.len())
        .filter(|&row| {
            if query.is_empty() {
                return true;
            }
            if row_label(world, &grid.rows[row]).to_lowercase().contains(&query) {
                return true;
            }
            shown.iter().any(|path| {
                read_cell_text(world, registry, grid, row, path)
                    .is_some_and(|text| text.to_lowercase().contains(&query))
            })
        })
        .collect();
    if let Some((path, descending)) = grid.sort.clone() {
        let texts: Vec<Option<String>> = (0..grid.rows.len())
            .map(|row| read_cell_text(world, registry, grid, row, &path))
            .collect();
        order.sort_by(|&a, &b| {
            let ordering = compare_cells(texts[a].as_deref(), texts[b].as_deref());
            if descending { ordering.reverse() } else { ordering }
        });
    }
    grid.view = ViewCache {
        key: Some(key),
        order,
    };
}

fn view_order(world: &World, grid: &mut Grid) -> Result<Vec<usize>, PanelError> {
    let registry = registry_of(world)?;
    refresh_view(world, &registry.read(), grid);
    Ok(grid.view.order.clone())
}

// ---------------------------------------------------------------------------------------------
// Opening

fn first_holder_value(world: &World, component: &str, rows: &[RowKey]) -> Option<Box<dyn PartialReflect>> {
    rows.iter().find_map(|key| match key {
        RowKey::Entity(bits) => {
            let entity = crate::command::resolve_entity(world, *bits)?;
            crate::command::read_property(world, entity, component, "").ok()
        }
        RowKey::Asset(_) => None,
    })
}

fn default_columns(available: &[String], wanted: Option<&[String]>) -> Result<Vec<Column>, PanelError> {
    let paths: Vec<String> = match wanted {
        Some(wanted) => {
            for path in wanted {
                if !available.contains(path) {
                    return Err(err(
                        "bulk_edit_bad_column",
                        format!("`{path}` is not a column; columns: {}", available.join(", ")),
                    ));
                }
            }
            wanted.to_vec()
        }
        None => available.to_vec(),
    };
    Ok(paths
        .into_iter()
        .map(|path| Column {
            path,
            width: DEFAULT_WIDTH,
            pinned: false,
        })
        .collect())
}

fn new_grid(
    source: RowSource,
    component: String,
    rows: Vec<RowKey>,
    docs: Vec<DataAssetDoc>,
    available: Vec<String>,
    columns: Vec<Column>,
) -> Grid {
    Grid {
        source,
        component,
        rows,
        docs,
        available,
        columns,
        sort: None,
        filter: String::new(),
        cursor: (0, 0),
        anchor: None,
        first_column: 0,
        view: ViewCache::default(),
    }
}

/// The reflected components every one of `entities` has (full type paths, sorted by short name).
#[must_use]
pub fn shared_components(world: &World, entities: &[Entity]) -> Vec<String> {
    let Some(registry) = world.get_resource::<AppTypeRegistry>() else {
        return Vec::new();
    };
    let registry = registry.read();
    let mut shared: Option<BTreeSet<String>> = None;
    for entity in entities {
        let Ok(infos) = world.inspect_entity(*entity) else {
            continue;
        };
        let names: BTreeSet<String> = infos
            .map(|(_, info)| info.name().to_string())
            .filter(|name| {
                registry
                    .get_with_type_path(name)
                    .is_some_and(|r| r.data::<ReflectComponent>().is_some())
            })
            .collect();
        shared = Some(match shared {
            None => names,
            Some(previous) => previous.intersection(&names).cloned().collect(),
        });
    }
    let mut out: Vec<String> = shared.unwrap_or_default().into_iter().collect();
    out.sort_by_key(|path| path.rsplit("::").next().unwrap_or(path).to_owned());
    out
}

/// Opens a grid of `entities` by `component`'s fields (`None`: the open grid's component when all
/// have it, else the first component they share). `columns` picks the shown columns.
///
/// # Errors
/// `bulk_edit_no_rows`, `bulk_edit_entity_gone`, `bulk_edit_unknown_component`,
/// `bulk_edit_component_on_none`, `bulk_edit_bad_column`, `bulk_edit_unsaved`.
pub fn open_entities(
    world: &mut World,
    entities: &[Entity],
    component: Option<&str>,
    columns: Option<&[String]>,
) -> Result<(), PanelError> {
    if entities.is_empty() {
        return Err(err("bulk_edit_no_rows", "no entities to edit; select some first"));
    }
    for entity in entities {
        if world.get_entity(*entity).is_err() {
            return Err(err(
                "bulk_edit_entity_gone",
                format!("entity {} does not exist", entity.to_bits()),
            ));
        }
    }
    refuse_unsaved(world)?;
    let registry = registry_of(world)?;
    let full = {
        let registry = registry.read();
        match component {
            Some(name) => component_type_path(&registry, name).ok_or_else(|| {
                err(
                    "bulk_edit_unknown_component",
                    format!("`{name}` is not a registered reflected component"),
                )
            })?,
            None => {
                drop(registry);
                let shared = shared_components(world, entities);
                let previous = world
                    .get_resource::<BulkEdit>()
                    .and_then(|b| b.grid.as_ref())
                    .filter(|g| g.source == RowSource::Entities)
                    .map(|g| g.component.clone());
                previous
                    .filter(|p| shared.contains(p))
                    .or_else(|| shared.first().cloned())
                    .ok_or_else(|| {
                        err(
                            "bulk_edit_component_on_none",
                            "the entities share no reflected component",
                        )
                    })?
            }
        }
    };
    let rows: Vec<RowKey> = entities.iter().map(|e| RowKey::Entity(e.to_bits())).collect();
    let sample = first_holder_value(world, &full, &rows).ok_or_else(|| {
        err(
            "bulk_edit_component_on_none",
            format!("none of the entities has {full}"),
        )
    })?;
    let available = leaf_paths(sample.as_ref());
    let columns = default_columns(&available, columns)?;
    let grid = new_grid(
        RowSource::Entities,
        full.clone(),
        rows,
        Vec::new(),
        available,
        columns,
    );
    install_grid(
        world,
        grid,
        format!("{} entities by {}", entities.len(), short(&full)),
    );
    Ok(())
}

fn short(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

/// Opens a grid of the data assets at `paths` (project-relative, `assets/...`), which must all be
/// of one type (`type_name`, when given, must be it).
///
/// # Errors
/// `bulk_edit_no_rows`, `bulk_edit_no_project`, `bulk_edit_mixed_types`, `bulk_edit_bad_column`,
/// `bulk_edit_unsaved` and the `data_asset_*` codes of reading a file.
pub fn open_assets(
    world: &mut World,
    paths: &[String],
    type_name: Option<&str>,
    columns: Option<&[String]>,
) -> Result<(), PanelError> {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| err("bulk_edit_no_project", "no project is open"))?;
    open_assets_at(world, &root, paths, type_name, columns)
}

fn open_assets_at(
    world: &mut World,
    root: &Path,
    paths: &[String],
    type_name: Option<&str>,
    columns: Option<&[String]>,
) -> Result<(), PanelError> {
    if paths.is_empty() {
        return Err(err("bulk_edit_no_rows", "no assets to edit"));
    }
    refuse_unsaved(world)?;
    let registry = registry_of(world)?;
    let registry = registry.read();
    let mut docs = Vec::with_capacity(paths.len());
    for path in paths {
        docs.push(DataAssetDoc::open(root, path, &registry)?);
    }
    let kind = docs[0].type_path.clone();
    if let Some(odd) = docs.iter().find(|d| d.type_path != kind) {
        return Err(err(
            "bulk_edit_mixed_types",
            format!(
                "{} is a {}, {} is a {}",
                docs[0].path, kind, odd.path, odd.type_path
            ),
        ));
    }
    if let Some(wanted) = type_name
        && !wanted.is_empty()
        && wanted != kind
        && wanted != short(&kind)
    {
        return Err(err(
            "bulk_edit_mixed_types",
            format!("the assets are {kind}, not {wanted}"),
        ));
    }
    let available = leaf_paths(docs[0].value().as_partial_reflect());
    drop(registry);
    let columns = default_columns(&available, columns)?;
    let rows = docs.iter().map(|d| RowKey::Asset(d.path.clone())).collect();
    let count = docs.len();
    let grid = new_grid(RowSource::Assets, kind.clone(), rows, docs, available, columns);
    install_grid(world, grid, format!("{count} assets of {}", short(&kind)));
    Ok(())
}

/// The `.data.ron` assets in the folder of `rel` (project-relative) with the same type as it,
/// sorted by path: the Content Browser's **Edit in grid** rows.
///
/// # Errors
/// `bulk_edit_no_project`, `bulk_edit_io`, or the `data_asset_*` codes of reading `rel`.
pub fn sibling_assets(root: &Path, rel: &str) -> Result<Vec<String>, PanelError> {
    let rel = rel.replace('\\', "/");
    let rel = if rel.starts_with("assets/") {
        rel
    } else {
        format!("assets/{rel}")
    };
    let read_type = |path: &Path| -> Option<String> {
        let text = std::fs::read_to_string(path).ok()?;
        DataAssetFile::from_ron_str(&text).ok().map(|f| f.type_path)
    };
    let full = root.join(&rel);
    let kind =
        read_type(&full).ok_or_else(|| err("data_asset_parse", format!("cannot read the type of {rel}")))?;
    let folder = rel.rsplit_once('/').map_or("", |(dir, _)| dir).to_owned();
    let entries = std::fs::read_dir(root.join(&folder))
        .map_err(|e| err("bulk_edit_io", format!("cannot list {folder}: {e}")))?;
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name.ends_with(&format!(".{DATA_ASSET_EXTENSION}"))
                && read_type(&entry.path()).as_deref() == Some(kind.as_str()))
            .then(|| format!("{folder}/{name}"))
        })
        .collect();
    out.sort();
    Ok(out)
}

fn refuse_unsaved(world: &World) -> Result<(), PanelError> {
    let Some(grid) = world.get_resource::<BulkEdit>().and_then(|b| b.grid.as_ref()) else {
        return Ok(());
    };
    let Ok(registry) = registry_of(world) else {
        return Ok(());
    };
    match unsaved_message(grid, &registry.read()) {
        Some(message) => Err(err("bulk_edit_unsaved", message)),
        None => Ok(()),
    }
}

/// Why `grid` may not close without discarding: the first asset with unsaved edits, or `None`.
fn unsaved_message(grid: &Grid, registry: &TypeRegistry) -> Option<String> {
    grid.docs.iter().find(|d| d.dirty(registry)).map(|doc| {
        format!(
            "{} has unsaved edits; save the grid (editor.bulk_edit_save) or close it with discard",
            doc.path
        )
    })
}

fn install_grid(world: &mut World, grid: Grid, what: String) {
    let mut panel = world.get_resource_or_insert_with(BulkEdit::default);
    panel.grid = Some(grid);
    panel.edit = None;
    panel.status = format!("Opened {what}");
    crate::editor_layout::LayoutRequests::push(
        world,
        crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::BulkEdit),
    );
}

// ---------------------------------------------------------------------------------------------
// Writing

/// What a commit did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitReport {
    /// Cells written to (including those already holding the value).
    pub set: usize,
    /// Cells whose value changed.
    pub changed: usize,
    /// Cells on rows without the component (left alone).
    pub skipped: usize,
    /// Pasted cells that fell outside the grid.
    pub clipped: usize,
    /// Undo entries after the commit.
    pub undo_depth: usize,
}

impl CommitReport {
    fn json(&self) -> Value {
        json!({
            "set": self.set,
            "changed": self.changed,
            "skipped": self.skipped,
            "clipped": self.clipped,
            "undo_depth": self.undo_depth,
        })
    }
}

/// One cell write: data row, column path, the input.
struct CellWrite {
    row: usize,
    path: String,
    input: CellInput,
}

/// One asset edit: every cell's value before and after (the reflect serializer's JSON).
struct BulkAssetEdit {
    label: String,
    cells: Vec<AssetCell>,
    coalesce: bool,
}

#[derive(Clone)]
struct AssetCell {
    path: String,
    field: String,
    before: Value,
    after: Value,
}

impl BulkAssetEdit {
    fn write(world: &mut World, cells: &[AssetCell], after: bool) {
        let Ok(registry) = registry_of(world) else {
            return;
        };
        let registry = registry.read();
        let Some(mut panel) = world.get_resource_mut::<BulkEdit>() else {
            return;
        };
        let Some(grid) = panel.grid.as_mut() else {
            return;
        };
        for cell in cells {
            let value = if after { &cell.after } else { &cell.before };
            if let Some(doc) = grid.docs.iter_mut().find(|d| d.path == cell.path)
                && let Err(e) = doc.set_field(&cell.field, value, &registry)
            {
                tracing::warn!(target: "regia_editor", "bulk edit on {}: {}", cell.path, e.message);
            }
        }
    }
}

impl EditorCommand for BulkAssetEdit {
    fn apply(&mut self, world: &mut World) {
        Self::write(world, &self.cells, true);
    }
    fn revert(&mut self, world: &mut World) {
        let reversed: Vec<AssetCell> = self.cells.iter().rev().cloned().collect();
        Self::write(world, &reversed, false);
    }
    fn merge(&mut self, next: &dyn EditorCommand) -> bool {
        let Some(next) = crate::command::downcast_to::<BulkAssetEdit>(next) else {
            return false;
        };
        let same = self.coalesce
            && next.coalesce
            && self.cells.len() == 1
            && next.cells.len() == 1
            && self.cells[0].path == next.cells[0].path
            && self.cells[0].field == next.cells[0].field;
        if same {
            self.cells[0].after = next.cells[0].after.clone();
        }
        same
    }
    fn changes_level_state(&self) -> bool {
        // The assets keep their own dirty state; Save writes them.
        false
    }
    fn description(&self) -> String {
        self.label.clone()
    }
}

fn take_grid(world: &mut World) -> Result<Grid, PanelError> {
    world
        .get_resource_mut::<BulkEdit>()
        .and_then(|mut panel| panel.grid.take())
        .ok_or_else(|| {
            err(
                "bulk_edit_not_open",
                "no grid is open; call editor.bulk_edit_open first",
            )
        })
}

fn put_grid(world: &mut World, grid: Grid) {
    world.get_resource_or_insert_with(BulkEdit::default).grid = Some(grid);
}

/// Writes `writes` as one undo step (entity rows: [`SetProperty`] commands, grouped; asset rows:
/// one asset edit). Every value is checked before anything is written.
fn commit(
    world: &mut World,
    grid: &mut Grid,
    writes: Vec<CellWrite>,
    label: &str,
    coalesce: bool,
) -> Result<CommitReport, PanelError> {
    let registry = registry_of(world)?;
    let mut report = CommitReport::default();
    match grid.source {
        RowSource::Entities => {
            let mut commands: Vec<SetProperty> = Vec::new();
            {
                let registry = registry.read();
                for write in &writes {
                    let Some(RowKey::Entity(bits)) = grid.rows.get(write.row) else {
                        return Err(err(
                            "bulk_edit_bad_row",
                            format!("row {} is not in the grid", write.row),
                        ));
                    };
                    let entity = crate::command::resolve_entity(world, *bits).ok_or_else(|| {
                        err("bulk_edit_entity_gone", format!("entity {bits} does not exist"))
                    })?;
                    let Ok(current) =
                        crate::command::read_property(world, entity, &grid.component, &write.path)
                    else {
                        report.skipped += 1;
                        continue;
                    };
                    regia_agent::require_not_feed_owned(world, entity)
                        .map_err(|e| err("bulk_edit_feed_owned", e.message))?;
                    let next = parse_cell(&registry, current.as_ref(), &write.input).map_err(|why| {
                        err(
                            "bulk_edit_bad_value",
                            format!(
                                "{} on {}: {why}",
                                write.path,
                                row_label(world, &grid.rows[write.row])
                            ),
                        )
                    })?;
                    report.set += 1;
                    if current.reflect_partial_eq(next.as_ref()) == Some(true) {
                        continue;
                    }
                    let mut command = SetProperty::capture(world, entity, &grid.component, &write.path, next)
                        .map_err(|e| err("bulk_edit_bad_value", e.to_string()))?;
                    command.coalesce = coalesce;
                    commands.push(command);
                }
            }
            report.changed = commands.len();
            let mut boxed: Vec<Box<dyn EditorCommand>> = Vec::with_capacity(commands.len());
            for mut command in commands {
                command.apply(world);
                boxed.push(Box::new(command));
            }
            report.undo_depth = push(world, boxed, label, coalesce);
        }
        RowSource::Assets => {
            let mut cells = Vec::new();
            {
                let registry = registry.read();
                for write in &writes {
                    let doc = grid.docs.get(write.row).ok_or_else(|| {
                        err(
                            "bulk_edit_bad_row",
                            format!("row {} is not in the grid", write.row),
                        )
                    })?;
                    let Some(current) = field_at(doc.value().as_partial_reflect(), &write.path) else {
                        report.skipped += 1;
                        continue;
                    };
                    let next = parse_cell(&registry, current, &write.input).map_err(|why| {
                        err(
                            "bulk_edit_bad_value",
                            format!("{} on {}: {why}", write.path, doc.path),
                        )
                    })?;
                    report.set += 1;
                    if current.reflect_partial_eq(next.as_ref()) == Some(true) {
                        continue;
                    }
                    let to_json = |value: &dyn PartialReflect| {
                        serde_json::to_value(TypedReflectSerializer::new(value, &registry))
                            .map_err(|e| err("bulk_edit_bad_value", e.to_string()))
                    };
                    cells.push(AssetCell {
                        path: doc.path.clone(),
                        field: write.path.clone(),
                        before: to_json(current)?,
                        after: to_json(next.as_ref())?,
                    });
                }
                // Apply in place; a refusal rolls the applied cells back.
                for (index, cell) in cells.iter().enumerate() {
                    let result = grid
                        .docs
                        .iter_mut()
                        .find(|d| d.path == cell.path)
                        .map(|doc| doc.set_field(&cell.field, &cell.after, &registry));
                    if let Some(Err(e)) = result {
                        for undo in cells[..index].iter().rev() {
                            if let Some(doc) = grid.docs.iter_mut().find(|d| d.path == undo.path) {
                                let _ = doc.set_field(&undo.field, &undo.before, &registry);
                            }
                        }
                        return Err(err("bulk_edit_bad_value", e.message));
                    }
                }
            }
            report.changed = cells.len();
            report.undo_depth = if cells.is_empty() {
                world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth)
            } else {
                let command = Box::new(BulkAssetEdit {
                    label: label.to_owned(),
                    cells,
                    coalesce,
                });
                push(world, vec![command], label, coalesce)
            };
        }
    }
    grid.view.key = None;
    Ok(report)
}

fn push(world: &mut World, commands: Vec<Box<dyn EditorCommand>>, label: &str, coalesce: bool) -> usize {
    let mut commands = commands.into_iter();
    let Some(first) = commands.next() else {
        return world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth);
    };
    let command: Box<dyn EditorCommand> = match commands.next() {
        None => first,
        Some(second) => {
            let mut group = CommandGroup::new(label);
            group.push(first);
            group.push(second);
            for command in commands {
                group.push(command);
            }
            Box::new(group)
        }
    };
    if coalesce {
        // A widget drag in one cell: the next frame of the same drag merges into this entry.
        let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
        stack.push(command);
        stack.undo_depth()
    } else {
        crate::undo_agent::push_discrete(world, command)
    }
}

/// Pastes tab-separated `tsv` with its top-left cell at view `(row, column)`, as one undo step.
/// Cells past the grid's last row or column are clipped (counted).
///
/// # Errors
/// `bulk_edit_not_open`, `bulk_edit_bad_row`, `bulk_edit_bad_column`, `bulk_edit_bad_value`
/// (nothing is written), `bulk_edit_entity_gone`, `bulk_edit_feed_owned`.
pub fn paste_tsv(
    world: &mut World,
    tsv: &str,
    row: usize,
    column: usize,
) -> Result<CommitReport, PanelError> {
    let mut grid = take_grid(world)?;
    let result = paste_into(world, &mut grid, tsv, row, column);
    put_grid(world, grid);
    result
}

fn paste_into(
    world: &mut World,
    grid: &mut Grid,
    tsv: &str,
    row: usize,
    column: usize,
) -> Result<CommitReport, PanelError> {
    let order = view_order(world, grid)?;
    let shown: Vec<String> = grid.shown().iter().map(|c| c.path.clone()).collect();
    check_cell(order.len(), shown.len(), row, column)?;
    let block = parse_tsv(tsv);
    let mut writes = Vec::new();
    let mut clipped = 0;
    for (dy, cells) in block.iter().enumerate() {
        for (dx, text) in cells.iter().enumerate() {
            match (order.get(row + dy), shown.get(column + dx)) {
                (Some(&data_row), Some(path)) => writes.push(CellWrite {
                    row: data_row,
                    path: path.clone(),
                    input: CellInput::Text(text.clone()),
                }),
                _ => clipped += 1,
            }
        }
    }
    let label = format!("Paste {} cells", writes.len());
    let mut report = commit(world, grid, writes, &label, false)?;
    report.clipped = clipped;
    Ok(report)
}

fn check_cell(rows: usize, columns: usize, row: usize, column: usize) -> Result<(), PanelError> {
    if row >= rows {
        return Err(err(
            "bulk_edit_bad_row",
            format!("row {row} is outside the {rows} shown rows"),
        ));
    }
    if column >= columns {
        return Err(err(
            "bulk_edit_bad_column",
            format!("column {column} is outside the {columns} shown columns"),
        ));
    }
    Ok(())
}

/// Copies the top row of `range` into the rows below it, as one undo step.
///
/// # Errors
/// As [`paste_tsv`].
pub fn fill_down(world: &mut World, range: CellRange) -> Result<CommitReport, PanelError> {
    let mut grid = take_grid(world)?;
    let result = fill_into(world, &mut grid, range);
    put_grid(world, grid);
    result
}

fn fill_into(world: &mut World, grid: &mut Grid, range: CellRange) -> Result<CommitReport, PanelError> {
    let order = view_order(world, grid)?;
    let shown: Vec<String> = grid.shown().iter().map(|c| c.path.clone()).collect();
    check_cell(order.len(), shown.len(), range.row, range.column)?;
    let rows = range.rows.max(1).min(order.len() - range.row);
    let columns = range.columns.max(1).min(shown.len() - range.column);
    let mut writes = Vec::new();
    for path in &shown[range.column..range.column + columns] {
        let Some(top) = read_cell(world, grid, order[range.row], path) else {
            continue;
        };
        for &data_row in &order[range.row + 1..range.row + rows] {
            let Some(value) = crate::inspector::reflect_ui::clone_value(top.as_ref()) else {
                continue;
            };
            writes.push(CellWrite {
                row: data_row,
                path: path.clone(),
                input: CellInput::Value(value),
            });
        }
    }
    commit(world, grid, writes, "Fill down", false)
}

/// The cells of `range` as tab-separated text (an empty cell for a row without the component).
///
/// # Errors
/// `bulk_edit_not_open`, `bulk_edit_bad_row`, `bulk_edit_bad_column`.
pub fn copy_tsv(world: &mut World, range: CellRange) -> Result<String, PanelError> {
    let mut grid = take_grid(world)?;
    let result = copy_from(world, &mut grid, range);
    put_grid(world, grid);
    result
}

fn copy_from(world: &World, grid: &mut Grid, range: CellRange) -> Result<String, PanelError> {
    let order = view_order(world, grid)?;
    let shown: Vec<String> = grid.shown().iter().map(|c| c.path.clone()).collect();
    check_cell(order.len(), shown.len(), range.row, range.column)?;
    let registry = registry_of(world)?;
    let registry = registry.read();
    let rows = range.rows.max(1).min(order.len() - range.row);
    let columns = range.columns.max(1).min(shown.len() - range.column);
    let block: Vec<Vec<String>> = order[range.row..range.row + rows]
        .iter()
        .map(|&data_row| {
            shown[range.column..range.column + columns]
                .iter()
                .map(|path| read_cell_text(world, &registry, grid, data_row, path).unwrap_or_default())
                .collect()
        })
        .collect();
    Ok(write_tsv(&block))
}

/// Writes the dirty asset rows (atomically). Returns `(path, bytes)` per saved file.
///
/// # Errors
/// `bulk_edit_not_open`, `bulk_edit_not_assets`, `bulk_edit_no_project`, and the
/// `data_asset_*` codes of writing a file.
pub fn save_assets(world: &mut World) -> Result<Vec<(String, usize)>, PanelError> {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone());
    let registry = registry_of(world)?;
    let mut grid = take_grid(world)?;
    let result = (|| {
        if grid.source != RowSource::Assets {
            return Err(err(
                "bulk_edit_not_assets",
                "the grid shows entities; the level saves them",
            ));
        }
        let root = root.ok_or_else(|| err("bulk_edit_no_project", "no project is open"))?;
        let registry = registry.read();
        let mut saved = Vec::new();
        for doc in &mut grid.docs {
            if doc.dirty(&registry) {
                let bytes = doc.save(&root, &registry)?;
                saved.push((doc.path.clone(), bytes));
            }
        }
        Ok(saved)
    })();
    put_grid(world, grid);
    result
}

// ---------------------------------------------------------------------------------------------
// State

fn state_json(
    world: &mut World,
    offset: usize,
    limit: usize,
    range: Option<CellRange>,
) -> Result<Value, PanelError> {
    let mut grid = take_grid(world)?;
    let result = (|| {
        let order = view_order(world, &mut grid)?;
        let registry = registry_of(world)?;
        let registry = registry.read();
        let shown: Vec<String> = grid.shown().iter().map(|c| c.path.clone()).collect();
        let rows: Vec<Value> = order
            .iter()
            .enumerate()
            .skip(offset)
            .take(limit)
            .map(|(view_row, &data_row)| {
                let cells: Vec<Value> = shown
                    .iter()
                    .map(
                        |path| match read_cell_text(world, &registry, &grid, data_row, path) {
                            Some(text) => json!(text),
                            None => Value::Null,
                        },
                    )
                    .collect();
                json!({
                    "row": view_row,
                    "key": grid.rows[data_row].json(),
                    "label": row_label(world, &grid.rows[data_row]),
                    "cells": cells,
                })
            })
            .collect();
        let dirty: Vec<&str> = grid
            .docs
            .iter()
            .filter(|d| d.dirty(&registry))
            .map(|d| d.path.as_str())
            .collect();
        let columns: Vec<Value> = grid
            .shown()
            .iter()
            .map(|c| json!({ "path": c.path, "pinned": c.pinned, "width": c.width }))
            .collect();
        let drawn = world.get_resource::<BulkEdit>().map_or(0, |b| b.drawn_rows);
        let mut out = json!({
            "source": grid.source.as_str(),
            "component": grid.component,
            "columns": columns,
            "available": grid.available,
            "sort": grid.sort.as_ref().map(|(column, descending)| json!({ "column": column, "descending": descending })),
            "filter": grid.filter,
            "rows_total": grid.rows.len(),
            "rows_shown": order.len(),
            "offset": offset,
            "rows": rows,
            "cursor": { "row": grid.cursor.0, "column": grid.cursor.1 },
            "range": grid.range().json(),
            "drawn_rows": drawn,
            "dirty": dirty,
        });
        drop(registry);
        if let Some(range) = range {
            out["tsv"] = json!(copy_from(world, &mut grid, range)?);
        }
        Ok(out)
    })();
    put_grid(world, grid);
    result
}

// ---------------------------------------------------------------------------------------------
// Agent methods

fn bad(message: impl Into<String>) -> RpcError {
    err("bulk_edit_bad_params", message).rpc(HINT)
}

fn usize_param(params: &Value, name: &str) -> Result<Option<usize>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| bad(format!("`{name}` is a non-negative integer"))),
    }
}

fn strings_param(params: &Value, name: &str) -> Result<Option<Vec<String>>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or_else(|| bad(format!("`{name}` is an array of strings"))),
        Some(_) => Err(bad(format!("`{name}` is an array of strings"))),
    }
}

fn range_param(value: &Value, what: &str) -> Result<CellRange, RpcError> {
    let field = |name: &str, default: Option<usize>| -> Result<usize, RpcError> {
        match value.get(name) {
            None | Some(Value::Null) => default.ok_or_else(|| bad(format!("`{what}.{name}` is required"))),
            Some(v) => v
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| bad(format!("`{what}.{name}` is a non-negative integer"))),
        }
    };
    Ok(CellRange {
        row: field("row", None)?,
        column: field("column", None)?,
        rows: field("rows", Some(1))?.max(1),
        columns: field("columns", Some(1))?.max(1),
    })
}

/// `editor.bulk_edit_open`.
fn open_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let component = match params.get("component") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| bad("`component` is a string"))?
                .to_owned(),
        ),
    };
    let columns = strings_param(params, "columns")?;
    let assets = strings_param(params, "assets")?;
    let entities = match params.get("entities") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|v| v.as_u64())
                .collect::<Option<Vec<u64>>>()
                .ok_or_else(|| bad("`entities` is an array of entity ids"))?,
        ),
        Some(_) => return Err(bad("`entities` is an array of entity ids")),
    };
    if params.get("discard").and_then(Value::as_bool) == Some(true)
        && let Some(mut panel) = world.get_resource_mut::<BulkEdit>()
    {
        panel.grid = None;
    }
    match (entities, assets) {
        (Some(_), Some(_)) => return Err(bad("give `entities` or `assets`, not both")),
        (None, Some(paths)) => {
            open_assets(world, &paths, component.as_deref(), columns.as_deref()).map_err(|e| e.rpc(HINT))?;
        }
        (entities, None) => {
            let entities: Vec<Entity> = match entities {
                Some(bits) => bits
                    .into_iter()
                    .map(|b| {
                        crate::command::resolve_entity(world, b).ok_or_else(|| {
                            err("bulk_edit_entity_gone", format!("entity {b} does not exist")).rpc(HINT)
                        })
                    })
                    .collect::<Result<_, _>>()?,
                None => world
                    .get_resource::<crate::selection::Selection>()
                    .map(|s| s.entities().to_vec())
                    .unwrap_or_default(),
            };
            open_entities(world, &entities, component.as_deref(), columns.as_deref())
                .map_err(|e| e.rpc(HINT))?;
        }
    }
    state_json(world, 0, DEFAULT_LIMIT, None).map_err(|e| e.rpc(HINT))
}

/// `editor.bulk_edit_set`.
fn set_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let modes = ["cells", "tsv", "fill_down"]
        .iter()
        .filter(|name| params.get(**name).is_some_and(|v| !v.is_null()))
        .count();
    if modes != 1 {
        return Err(bad("give exactly one of `cells`, `tsv` or `fill_down`"));
    }
    let report = if let Some(tsv) = params.get("tsv").filter(|v| !v.is_null()) {
        let tsv = tsv.as_str().ok_or_else(|| bad("`tsv` is a string"))?;
        let row = usize_param(params, "row")?.unwrap_or(0);
        let column = usize_param(params, "column")?.unwrap_or(0);
        paste_tsv(world, tsv, row, column).map_err(|e| e.rpc(HINT))?
    } else if let Some(range) = params.get("fill_down").filter(|v| !v.is_null()) {
        fill_down(world, range_param(range, "fill_down")?).map_err(|e| e.rpc(HINT))?
    } else {
        let cells = params
            .get("cells")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("`cells` is an array of { row | key, column, value | text }"))?;
        set_cells(world, cells)?
    };
    Ok(report.json())
}

fn set_cells(world: &mut World, cells: &[Value]) -> Result<CommitReport, RpcError> {
    let mut grid = take_grid(world).map_err(|e| e.rpc(HINT))?;
    let result = (|| -> Result<CommitReport, RpcError> {
        let order = view_order(world, &mut grid).map_err(|e| e.rpc(HINT))?;
        let shown: Vec<String> = grid.shown().iter().map(|c| c.path.clone()).collect();
        let mut writes = Vec::with_capacity(cells.len());
        for cell in cells {
            let data_row = match (cell.get("row"), cell.get("key")) {
                (Some(row), None) => {
                    let row = row
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or_else(|| bad("`row` is a view row index"))?;
                    *order.get(row).ok_or_else(|| {
                        err(
                            "bulk_edit_bad_row",
                            format!("row {row} is outside the {} shown rows", order.len()),
                        )
                        .rpc(HINT)
                    })?
                }
                (None, Some(key)) => grid.rows.iter().position(|k| k.json() == *key).ok_or_else(|| {
                    err("bulk_edit_bad_row", format!("{key} is not a row of the grid")).rpc(HINT)
                })?,
                _ => {
                    return Err(bad(
                        "each cell has `row` (view index) or `key` (entity id or asset path)",
                    ));
                }
            };
            let path = match cell.get("column") {
                Some(Value::String(path)) if grid.available.contains(path) => path.clone(),
                Some(Value::Number(n)) => n
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .and_then(|n| shown.get(n).cloned())
                    .ok_or_else(|| {
                        err("bulk_edit_bad_column", format!("column {n} is not shown")).rpc(HINT)
                    })?,
                Some(other) => {
                    return Err(err("bulk_edit_bad_column", format!("{other} is not a column")).rpc(HINT));
                }
                None => return Err(bad("each cell has `column` (a path or a shown column index)")),
            };
            let input = match (cell.get("value"), cell.get("text")) {
                (Some(value), None) => CellInput::Json(value.clone()),
                (None, Some(Value::String(text))) => CellInput::Text(text.clone()),
                _ => return Err(bad("each cell has `value` (JSON) or `text` (cell text)")),
            };
            writes.push(CellWrite {
                row: data_row,
                path,
                input,
            });
        }
        let label = format!("Set {} cells", writes.len());
        commit(world, &mut grid, writes, &label, false).map_err(|e| e.rpc(HINT))
    })();
    put_grid(world, grid);
    result
}

/// `editor.bulk_edit_state`.
fn state_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let offset = usize_param(params, "offset")?.unwrap_or(0);
    let limit = usize_param(params, "limit")?.unwrap_or(DEFAULT_LIMIT);
    let range = match params.get("range") {
        None | Some(Value::Null) => None,
        Some(range) => Some(range_param(range, "range")?),
    };
    state_json(world, offset, limit, range).map_err(|e| e.rpc(HINT))
}

/// `editor.bulk_edit_view`.
fn view_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if params.get("close").and_then(Value::as_bool) == Some(true) {
        let discard = params.get("discard").and_then(Value::as_bool) == Some(true);
        if !discard {
            refuse_unsaved(world).map_err(|e| e.rpc(HINT))?;
        }
        if let Some(mut panel) = world.get_resource_mut::<BulkEdit>() {
            panel.grid = None;
            panel.edit = None;
            "Closed".clone_into(&mut panel.status);
        }
        return Ok(json!({ "closed": true }));
    }
    let mut grid = take_grid(world).map_err(|e| e.rpc(HINT))?;
    let result = apply_view(&mut grid, params);
    put_grid(world, grid);
    result?;
    let offset = usize_param(params, "offset")?.unwrap_or(0);
    let limit = usize_param(params, "limit")?.unwrap_or(DEFAULT_LIMIT);
    state_json(world, offset, limit, None).map_err(|e| e.rpc(HINT))
}

fn apply_view(grid: &mut Grid, params: &Value) -> Result<(), RpcError> {
    let unknown = |path: &str| err("bulk_edit_bad_column", format!("`{path}` is not a column")).rpc(HINT);
    if let Some(columns) = strings_param(params, "columns")? {
        for path in &columns {
            if !grid.available.contains(path) {
                return Err(unknown(path));
            }
        }
        let old = std::mem::take(&mut grid.columns);
        grid.columns = columns
            .into_iter()
            .map(|path| {
                old.iter().find(|c| c.path == path).cloned().unwrap_or(Column {
                    path,
                    width: DEFAULT_WIDTH,
                    pinned: false,
                })
            })
            .collect();
    }
    if let Some(pinned) = strings_param(params, "pinned")? {
        for path in &pinned {
            if !grid.columns.iter().any(|c| c.path == *path) {
                return Err(unknown(path));
            }
        }
        for column in &mut grid.columns {
            column.pinned = pinned.contains(&column.path);
        }
    }
    match params.get("sort") {
        None => {}
        Some(Value::Null) => grid.sort = None,
        Some(sort) => {
            let column = sort
                .get("column")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("`sort.column` is a column path"))?;
            if !grid.columns.iter().any(|c| c.path == column) {
                return Err(unknown(column));
            }
            let descending = sort.get("descending").and_then(Value::as_bool).unwrap_or(false);
            grid.sort = Some((column.to_owned(), descending));
        }
    }
    if let Some(filter) = params.get("filter").filter(|v| !v.is_null()) {
        filter
            .as_str()
            .ok_or_else(|| bad("`filter` is a string"))?
            .clone_into(&mut grid.filter);
    }
    if let Some(widths) = params.get("widths").filter(|v| !v.is_null()) {
        let widths = widths
            .as_object()
            .ok_or_else(|| bad("`widths` is { path: points }"))?;
        for (path, width) in widths {
            let width = width
                .as_f64()
                .ok_or_else(|| bad("a width is a number of points"))?;
            let column = grid
                .columns
                .iter_mut()
                .find(|c| c.path == *path)
                .ok_or_else(|| unknown(path))?;
            column.width = (width as f32).clamp(MIN_WIDTH, MAX_WIDTH);
        }
    }
    if let Some(cursor) = params.get("cursor").filter(|v| !v.is_null()) {
        let range = range_param(cursor, "cursor")?;
        grid.cursor = (range.row, range.column);
        grid.anchor = (range.rows > 1 || range.columns > 1)
            .then(|| (range.row + range.rows - 1, range.column + range.columns - 1));
    }
    grid.view.key = None;
    Ok(())
}

/// `editor.bulk_edit_save`.
fn save_method(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let saved = save_assets(world).map_err(|e| e.rpc(HINT))?;
    let bytes: usize = saved.iter().map(|(_, b)| b).sum();
    let paths: Vec<&str> = saved.iter().map(|(p, _)| p.as_str()).collect();
    Ok(json!({ "saved": paths, "bytes": bytes }))
}

const P_RANGE: &str = "{ row, column, rows?, columns? }: view row and shown column of the top-left cell (rows and columns default to 1).";

/// The Bulk Edit agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.bulk_edit_open",
                tool: "bulk_edit_open",
                description: "Open the Bulk Edit grid: rows are entities (default: the selection) or .data.ron assets of one type, columns are the reflected fields of `component` (nested structs flattened to dotted paths such as offset.x). Opens the panel. Refused with bulk_edit_unsaved while asset rows have unsaved edits, unless discard: true.",
                params: &[
                    ParamSchema {
                        name: "entities",
                        kind: "array",
                        required: false,
                        description: "Entity ids (bits) for the rows; default the editor selection.",
                    },
                    ParamSchema {
                        name: "assets",
                        kind: "array",
                        required: false,
                        description: "Project-relative .data.ron paths for the rows, all of one type (instead of entities).",
                    },
                    ParamSchema {
                        name: "component",
                        kind: "string",
                        required: false,
                        description: "Component type (short or full path). Entities: default the open grid's component when all have it, else the first one they share. Assets: optional, must name their type.",
                    },
                    ParamSchema {
                        name: "columns",
                        kind: "array",
                        required: false,
                        description: "Column paths to show, in order (default every field; `available` in the result lists them).",
                    },
                    ParamSchema {
                        name: "discard",
                        kind: "boolean",
                        required: false,
                        description: "Drop the open grid's unsaved asset edits first.",
                    },
                ],
                result: "the editor.bulk_edit_state result",
                mutating: true,
                permission: Permission::Read,
            },
            open_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.bulk_edit_set",
                tool: "bulk_edit_set",
                description: "Write cells of the open grid as one undo step (editor.undo reverts all of them), checked before anything is written: one value that does not fit refuses the whole call (bulk_edit_bad_value). Give exactly one of `cells`, `tsv` (+ row, column: what Ctrl+V does) or `fill_down` (what Ctrl+D does). Rows without the component are skipped; pasted cells past the grid are clipped.",
                params: &[
                    ParamSchema {
                        name: "cells",
                        kind: "array",
                        required: false,
                        description: "[{ row (view index) | key (entity id or asset path), column (path or shown index), value (reflect JSON) | text (cell text) }].",
                    },
                    ParamSchema {
                        name: "tsv",
                        kind: "string",
                        required: false,
                        description: "Tab-separated rows of cell text (a spreadsheet's clipboard form; quoted cells may hold tabs and line breaks).",
                    },
                    ParamSchema {
                        name: "row",
                        kind: "integer",
                        required: false,
                        description: "With tsv: the view row of the top-left cell (default 0).",
                    },
                    ParamSchema {
                        name: "column",
                        kind: "integer",
                        required: false,
                        description: "With tsv: the shown column of the top-left cell (default 0).",
                    },
                    ParamSchema {
                        name: "fill_down",
                        kind: "object",
                        required: false,
                        description: P_RANGE,
                    },
                ],
                result: "{ set, changed, skipped, clipped, undo_depth }",
                mutating: true,
                permission: Permission::EditScene,
            },
            set_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.bulk_edit_state",
                tool: "bulk_edit_state",
                description: "The open grid: columns in draw order (pinned first), every available column, sort, filter, row counts, a window of rows in view order with each cell's text (null where the row lacks the component), the cursor and range, drawn_rows (rows the panel laid out last frame) and dirty asset rows. With `range`, also `tsv`: the cells as Ctrl+C copies them.",
                params: &[
                    ParamSchema {
                        name: "offset",
                        kind: "integer",
                        required: false,
                        description: "First view row returned (default 0).",
                    },
                    ParamSchema {
                        name: "limit",
                        kind: "integer",
                        required: false,
                        description: "Rows returned (default 100).",
                    },
                    ParamSchema {
                        name: "range",
                        kind: "object",
                        required: false,
                        description: P_RANGE,
                    },
                ],
                result: "{ source, component, columns: [{ path, pinned, width }], available, sort, filter, rows_total, rows_shown, offset, rows: [{ row, key, label, cells }], cursor, range, drawn_rows, dirty, tsv? }",
                mutating: false,
                permission: Permission::Read,
            },
            state_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.bulk_edit_view",
                tool: "bulk_edit_view",
                description: "Change what the grid shows, as the panel's column picker, header clicks, filter box and keyboard do: columns, pinned columns, sort, filter, widths and the cursor. close: true closes the grid (refused with bulk_edit_unsaved while asset rows have unsaved edits, unless discard: true). Nothing here is an undo step.",
                params: &[
                    ParamSchema {
                        name: "columns",
                        kind: "array",
                        required: false,
                        description: "Shown column paths, in order.",
                    },
                    ParamSchema {
                        name: "pinned",
                        kind: "array",
                        required: false,
                        description: "Shown columns kept left and in view (replaces the pinned set).",
                    },
                    ParamSchema {
                        name: "sort",
                        kind: "object",
                        required: false,
                        description: "{ column, descending? } or null for the opened order. Numbers sort by value, other cells by text; rows without the component last.",
                    },
                    ParamSchema {
                        name: "filter",
                        kind: "string",
                        required: false,
                        description: "Show rows whose label or a shown cell contains this text (any case); empty shows all.",
                    },
                    ParamSchema {
                        name: "widths",
                        kind: "object",
                        required: false,
                        description: "{ path: points } column widths (36 to 640).",
                    },
                    ParamSchema {
                        name: "cursor",
                        kind: "object",
                        required: false,
                        description: P_RANGE,
                    },
                    ParamSchema {
                        name: "close",
                        kind: "boolean",
                        required: false,
                        description: "Close the grid.",
                    },
                    ParamSchema {
                        name: "discard",
                        kind: "boolean",
                        required: false,
                        description: "With close: drop unsaved asset edits.",
                    },
                ],
                result: "the editor.bulk_edit_state result, or { closed: true }",
                mutating: false,
                permission: Permission::Read,
            },
            view_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.bulk_edit_save",
                tool: "bulk_edit_save",
                description: "Write the asset rows with unsaved edits to their .data.ron files (atomically). Entity rows are saved with the level (bulk_edit_not_assets).",
                params: &[],
                result: "{ saved: [path], bytes }",
                mutating: true,
                permission: Permission::Assets,
            },
            save_method as Handler,
        ),
    ]
    .into_iter()
    .chain(selection_methods())
    .collect()
}

// ---------------------------------------------------------------------------------------------
// The panel

/// What a key does to the grid (pure, so it is tested without a window).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GridKey {
    /// Move the cursor by (rows, columns); `extend` keeps the range anchor.
    Move {
        /// Row step.
        dr: isize,
        /// Column step.
        dc: isize,
        /// Shift held: extend the range.
        extend: bool,
    },
    /// Next cell (wraps to the next row).
    Next,
    /// Previous cell (wraps to the previous row).
    Previous,
    /// Edit the cursor cell as text.
    Edit,
    /// Fill the range down.
    FillDown,
    /// Change the cursor column's width.
    Width(f32),
    /// Leave the grid.
    Leave,
}

/// The grid action of a key press (`None`: not the grid's key).
#[must_use]
pub fn grid_key(key: egui::Key, modifiers: egui::Modifiers) -> Option<GridKey> {
    use egui::Key;
    let extend = modifiers.shift;
    if modifiers.alt {
        return match key {
            Key::ArrowLeft => Some(GridKey::Width(-WIDTH_STEP)),
            Key::ArrowRight => Some(GridKey::Width(WIDTH_STEP)),
            _ => None,
        };
    }
    if modifiers.command {
        return (key == Key::D).then_some(GridKey::FillDown);
    }
    Some(match key {
        Key::ArrowUp => GridKey::Move {
            dr: -1,
            dc: 0,
            extend,
        },
        Key::ArrowDown => GridKey::Move { dr: 1, dc: 0, extend },
        Key::ArrowLeft => GridKey::Move {
            dr: 0,
            dc: -1,
            extend,
        },
        Key::ArrowRight => GridKey::Move { dr: 0, dc: 1, extend },
        Key::Tab if modifiers.shift => GridKey::Previous,
        Key::Tab => GridKey::Next,
        Key::Enter | Key::F2 => GridKey::Edit,
        Key::Escape => GridKey::Leave,
        _ => return None,
    })
}

/// Moves `grid`'s cursor for `key` within `rows` x `columns`.
fn move_cursor(grid: &mut Grid, key: GridKey, rows: usize, columns: usize) {
    if rows == 0 || columns == 0 {
        return;
    }
    let (row, column) = grid.cursor;
    let step = |at: usize, by: isize, len: usize| at.saturating_add_signed(by).min(len - 1);
    match key {
        GridKey::Move { dr, dc, extend } => {
            if extend {
                grid.anchor.get_or_insert(grid.cursor);
            } else {
                grid.anchor = None;
            }
            grid.cursor = (step(row, dr, rows), step(column, dc, columns));
        }
        GridKey::Next => {
            grid.anchor = None;
            grid.cursor = if column + 1 < columns {
                (row, column + 1)
            } else if row + 1 < rows {
                (row + 1, 0)
            } else {
                (row, column)
            };
        }
        GridKey::Previous => {
            grid.anchor = None;
            grid.cursor = if column > 0 {
                (row, column - 1)
            } else if row > 0 {
                (row - 1, columns - 1)
            } else {
                (row, column)
            };
        }
        _ => {}
    }
}

/// The grid's keyboard focus target id.
#[must_use]
pub fn grid_focus_id() -> egui::Id {
    egui::Id::new("bulk_edit_grid_focus")
}

/// Draws the Bulk Edit panel.
#[tracing::instrument(target = "regia_editor", skip_all, name = "bulk_edit_panel")]
pub fn bulk_edit_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world.remove_resource::<BulkEdit>().unwrap_or_default();
    match panel.grid.take() {
        None => {
            draw_closed(ui, world, &mut panel);
        }
        Some(mut grid) => {
            let keep = draw_grid(ui, world, &mut panel, &mut grid);
            if keep {
                panel.grid = Some(grid);
            }
        }
    }
    // A method run inside the draw (an open from the closed view) may have inserted a fresh
    // panel: that one wins.
    if let Some(mut fresh) = world.remove_resource::<BulkEdit>() {
        fresh.drawn_rows = panel.drawn_rows;
        fresh.keys_frame = panel.keys_frame;
        panel = fresh;
    }
    world.insert_resource(panel);
}

fn draw_closed(ui: &mut egui::Ui, world: &mut World, panel: &mut BulkEdit) {
    ui.label("Select entities, then open them here, or right-click in the Outliner or on a data asset and choose Edit in grid.");
    let selected: Vec<Entity> = world
        .get_resource::<crate::selection::Selection>()
        .map(|s| s.entities().to_vec())
        .unwrap_or_default();
    if ui
        .add_enabled(
            !selected.is_empty(),
            egui::Button::new(format!("Open {} selected", selected.len())),
        )
        .clicked()
        && let Err(e) = open_entities(world, &selected, None, None)
    {
        panel.status = e.message;
    }
    if !panel.status.is_empty() {
        ui.weak(&panel.status);
    }
}

/// Draws an open grid; returns `false` when it was closed.
fn draw_grid(ui: &mut egui::Ui, world: &mut World, panel: &mut BulkEdit, grid: &mut Grid) -> bool {
    let Ok(registry) = registry_of(world) else {
        ui.label("The type registry is not available.");
        return true;
    };
    let mut closed = false;
    // Toolbar: component, filter, columns, actions.
    ui.horizontal_wrapped(|ui| {
        ui.strong(format!(
            "{} {} by {}",
            grid.rows.len(),
            grid.source.as_str(),
            short(&grid.component)
        ));
        if grid.source == RowSource::Entities {
            let entities: Vec<Entity> = grid
                .rows
                .iter()
                .filter_map(|k| match k {
                    RowKey::Entity(bits) => crate::command::resolve_entity(world, *bits),
                    RowKey::Asset(_) => None,
                })
                .collect();
            let mut switch_to = None;
            let combo = egui::ComboBox::from_id_salt("bulk_edit_component")
                .selected_text(short(&grid.component).to_owned())
                .show_ui(ui, |ui| {
                    for path in shared_components(world, &entities) {
                        if ui
                            .selectable_label(path == grid.component, short(&path))
                            .clicked()
                        {
                            switch_to = Some(path);
                        }
                    }
                });
            crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Component");
            if let Some(path) = switch_to.filter(|p| *p != grid.component) {
                match open_entities(world, &entities, Some(&path), None) {
                    Ok(()) => closed = true,
                    Err(e) => panel.status = e.message,
                }
            }
        }
        let filter = ui.add(
            egui::TextEdit::singleline(&mut grid.filter)
                .hint_text("Filter rows")
                .desired_width(160.0),
        );
        crate::a11y_audit::name(filter, egui::WidgetType::TextEdit, "Filter rows");
        let range = grid.range();
        if ui
            .add_enabled(range.rows > 1, egui::Button::new("Fill down"))
            .on_hover_text("Copy the top row of the range into the rows below (Ctrl+D)")
            .clicked()
        {
            match fill_into(world, grid, range) {
                Ok(report) => panel.status = format!("Filled {} cells", report.changed),
                Err(e) => panel.status = e.message,
            }
        }
        if ui
            .button("Copy")
            .on_hover_text("Copy the range as tab-separated text (Ctrl+C)")
            .clicked()
        {
            match copy_from(world, grid, range) {
                Ok(text) => ui.ctx().copy_text(text),
                Err(e) => panel.status = e.message,
            }
        }
        if grid.source == RowSource::Assets && ui.button("Save").clicked() {
            let mut docs = std::mem::take(&mut grid.docs);
            let saved = (|| {
                let root = world
                    .get_resource::<crate::agent_ext::EditorProject>()
                    .and_then(|p| p.root.clone())
                    .ok_or_else(|| err("bulk_edit_no_project", "no project is open"))?;
                let registry = registry.read();
                let mut count = 0;
                for doc in &mut docs {
                    if doc.dirty(&registry) {
                        doc.save(&root, &registry)?;
                        count += 1;
                    }
                }
                Ok::<_, PanelError>(count)
            })();
            grid.docs = docs;
            panel.status = match saved {
                Ok(count) => format!("Saved {count} assets"),
                Err(e) => e.message,
            };
        }
        // Close refuses a grid with unsaved asset edits, as `editor.bulk_edit_view { close }` does;
        // Discard and close is its `discard: true` (review sprint-w12-rev-g4 #1).
        if ui.button("Close").clicked() {
            match unsaved_message(grid, &registry.read()) {
                Some(message) => panel.status = message,
                None => {
                    closed = true;
                    panel.status = "Closed".to_owned();
                }
            }
        }
        if grid.source == RowSource::Assets
            && ui
                .button("Discard and close")
                .on_hover_text("Close the grid and drop its unsaved edits")
                .clicked()
        {
            closed = true;
            panel.status = "Closed; unsaved edits discarded".to_owned();
        }
    });
    if closed {
        return false;
    }
    draw_column_picker(ui, panel, grid);
    if !panel.status.is_empty() {
        ui.weak(&panel.status);
    }
    let order = {
        let registry = registry.read();
        refresh_view(world, &registry, grid);
        grid.view.order.clone()
    };
    let shown: Vec<Column> = grid.shown().into_iter().cloned().collect();
    if shown.is_empty() {
        ui.label("No columns: pick some under Columns.");
        return true;
    }
    grid.cursor.0 = grid.cursor.0.min(order.len().saturating_sub(1));
    grid.cursor.1 = grid.cursor.1.min(shown.len() - 1);
    // Horizontal scroll by column: keep the cursor's scrolling column in view.
    let pinned = shown.iter().filter(|c| c.pinned).count();
    let unpinned = shown.len() - pinned;
    let avail = ui.available_width() - LABEL_WIDTH - shown[..pinned].iter().map(|c| c.width).sum::<f32>();
    if grid.cursor.1 >= pinned {
        let at = grid.cursor.1 - pinned;
        if at < grid.first_column {
            grid.first_column = at;
        }
        while grid.first_column < at
            && shown[pinned + grid.first_column..=pinned + at]
                .iter()
                .map(|c| c.width)
                .sum::<f32>()
                > avail
        {
            grid.first_column += 1;
        }
    }
    grid.first_column = grid.first_column.min(unpinned.saturating_sub(1));
    let mut visible_columns: Vec<usize> = (0..pinned).collect();
    let mut used = 0.0;
    for (index, column) in shown.iter().enumerate().skip(pinned + grid.first_column) {
        if used > 0.0 && used + column.width > avail {
            break;
        }
        used += column.width;
        visible_columns.push(index);
    }
    // The keyboard focus target is the header's Row cell: Tab reaches it; while it has focus the
    // grid takes the arrows, Tab and Enter; Escape leaves.
    let row_cell = draw_header(ui, grid, &shown, &visible_columns);
    let focus = ui.interact(row_cell, grid_focus_id(), egui::Sense::focusable_noninteractive());
    let focus_label = format!(
        "Bulk Edit grid, {} rows, {} columns, cell {} {}",
        order.len(),
        shown.len(),
        grid.cursor.0 + 1,
        shown[grid.cursor.1].path
    );
    focus.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, &focus_label));
    let focused = focus.has_focus();
    if focused {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                grid_focus_id(),
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            );
        });
        ui.painter().rect_stroke(
            row_cell,
            0.0,
            ui.visuals().selection.stroke,
            egui::StrokeKind::Inside,
        );
        // The key that moved the focus here (Tab) is not a grid key.
        if !focus.gained_focus() {
            handle_keys(ui, world, panel, grid, order.len(), &shown);
        }
    }
    let row_height = ui.spacing().interact_size.y;
    let registry = registry.read();
    let range = grid.range();
    let mut drawn = 0;
    let mut cell_commit: Option<(usize, String, Box<dyn PartialReflect>, bool)> = None;
    let mut edit_commit: Option<(usize, usize, String, Option<GridKey>)> = None;
    let mut cursor_click = None;
    let mut cell_names: Vec<(egui::Rect, String)> = Vec::new();
    let layer = ui.layer_id();
    egui::ScrollArea::vertical()
        .id_salt("bulk_edit_rows")
        .auto_shrink([false, false])
        .show_rows(ui, row_height, order.len(), |ui, rows| {
            drawn = rows.len();
            for view_row in rows {
                let data_row = order[view_row];
                ui.horizontal(|ui| {
                    let label = row_label(world, &grid.rows[data_row]);
                    let response = ui.add_sized(
                        egui::vec2(LABEL_WIDTH, row_height),
                        egui::Label::new(label.as_str())
                            .truncate()
                            .sense(egui::Sense::click()),
                    );
                    if response.clicked() {
                        // A row label click moves the cursor to the row and gives the grid the keys.
                        cursor_click = Some((view_row, grid.cursor.1, true));
                    }
                    for &index in &visible_columns {
                        let column = &shown[index];
                        let (rect, _) = ui
                            .allocate_exact_size(egui::vec2(column.width, row_height), egui::Sense::hover());
                        if range.contains(view_row, index) {
                            ui.painter().rect_filled(
                                rect,
                                0.0,
                                ui.visuals().selection.bg_fill.gamma_multiply(0.35),
                            );
                        }
                        if (view_row, index) == grid.cursor {
                            ui.painter().rect_stroke(
                                rect,
                                0.0,
                                ui.visuals().selection.stroke,
                                egui::StrokeKind::Inside,
                            );
                        }
                        if ui.rect_contains_pointer(rect) && ui.input(|i| i.pointer.primary_pressed()) {
                            // The cell's widget keeps the press (a text field takes the keys).
                            cursor_click = Some((view_row, index, false));
                        }
                        cell_names.push((rect, format!("{label} {}", column.path)));
                        let mut cell_ui =
                            ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(1.0)).id_salt((
                                "bulk_cell",
                                data_row,
                                index,
                            )));
                        cell_ui.set_clip_rect(rect.intersect(ui.clip_rect()));
                        if let Some(edit) = panel
                            .edit
                            .as_mut()
                            .filter(|e| e.row == view_row && e.column == index)
                        {
                            let response = cell_ui
                                .add(egui::TextEdit::singleline(&mut edit.text).desired_width(rect.width()));
                            let response = crate::a11y_audit::name(
                                response,
                                egui::WidgetType::TextEdit,
                                &format!("Edit {}", column.path),
                            );
                            if !edit.focused {
                                response.request_focus();
                                edit.focused = true;
                            } else if response.lost_focus() {
                                let (enter, tab, shift, escape) = cell_ui.input(|i| {
                                    (
                                        i.key_pressed(egui::Key::Enter),
                                        i.key_pressed(egui::Key::Tab),
                                        i.modifiers.shift,
                                        i.key_pressed(egui::Key::Escape),
                                    )
                                });
                                let then = if enter {
                                    Some(GridKey::Move {
                                        dr: 1,
                                        dc: 0,
                                        extend: false,
                                    })
                                } else if tab && shift {
                                    Some(GridKey::Previous)
                                } else if tab {
                                    Some(GridKey::Next)
                                } else {
                                    None
                                };
                                if escape {
                                    edit_commit = Some((usize::MAX, index, String::new(), None));
                                } else {
                                    edit_commit = Some((data_row, index, edit.text.clone(), then));
                                }
                            }
                            continue;
                        }
                        match read_cell(world, grid, data_row, &column.path) {
                            None => {
                                cell_ui.weak("—");
                            }
                            Some(mut value) => {
                                let mut context = InspectorContext::new(grid.component.clone());
                                let changed = InspectorUi::new(&registry, &mut context)
                                    .ui_for_reflect_with_options(
                                        value.as_mut(),
                                        &mut cell_ui,
                                        egui::Id::new(("bulk_edit_cell", data_row, index)),
                                        &(),
                                    );
                                if changed {
                                    let coalesce = context.take_changes().iter().all(|c| !c.discrete);
                                    cell_commit = Some((data_row, column.path.clone(), value, coalesce));
                                }
                            }
                        }
                    }
                });
            }
        });
    drop(registry);
    name_cell_widgets(ui.ctx(), layer, &cell_names);
    // `show_rows` laid out these rows; egui may run the panel twice a frame, which sets the same
    // count again.
    panel.drawn_rows = drawn;
    if let Some((view_row, column, take_keys)) = cursor_click {
        grid.cursor = (view_row, column);
        grid.anchor = None;
        if take_keys && panel.edit.is_none() {
            focus.request_focus();
        }
    }
    if let Some((data_row, path, value, coalesce)) = cell_commit {
        let write = CellWrite {
            row: data_row,
            path: path.clone(),
            input: CellInput::Value(value),
        };
        if let Err(e) = commit(world, grid, vec![write], &format!("Set {path}"), coalesce) {
            panel.status = e.message;
        }
    }
    if let Some((data_row, index, text, then)) = edit_commit {
        panel.edit = None;
        if data_row != usize::MAX {
            let write = CellWrite {
                row: data_row,
                path: shown[index].path.clone(),
                input: CellInput::Text(text),
            };
            match commit(
                world,
                grid,
                vec![write],
                &format!("Set {}", shown[index].path),
                false,
            ) {
                Ok(_) => {
                    if let Some(key) = then {
                        move_cursor(grid, key, order.len(), shown.len());
                    }
                }
                Err(e) => panel.status = e.message,
            }
        }
        focus.request_focus();
    }
    true
}

/// Gives each cell's Inspector widget with no accessible name the name `<row> <column>` (the
/// Inspector names a field by the label beside it; a cell has none).
fn name_cell_widgets(ctx: &egui::Context, layer: egui::LayerId, cells: &[(egui::Rect, String)]) {
    if cells.is_empty() || ctx.viewport(|v| v.this_pass.accesskit_state.is_none()) {
        return;
    }
    let widgets: Vec<(egui::Id, egui::Pos2)> = ctx.viewport(|v| {
        v.this_pass
            .widgets
            .get_layer(layer)
            .map(|w| (w.id, w.rect.center()))
            .collect()
    });
    for (id, center) in widgets {
        let Some((_, name)) = cells.iter().find(|(rect, _)| rect.contains(center)) else {
            continue;
        };
        ctx.accesskit_node_builder(id, |node| {
            if node.label().is_none_or(|l| l.trim().is_empty()) && node.labelled_by().is_empty() {
                node.set_label(name.as_str());
            }
        });
    }
}

/// Draws the column headers; returns the Row cell's rect (the grid's focus target).
fn draw_header(ui: &mut egui::Ui, grid: &mut Grid, shown: &[Column], visible: &[usize]) -> egui::Rect {
    let height = ui.spacing().interact_size.y;
    ui.horizontal(|ui| {
        let row_cell = ui
            .add_sized(egui::vec2(LABEL_WIDTH, height), egui::Label::new(egui::RichText::new("Row").strong()))
            .rect;
        for &index in visible {
            let column = &shown[index];
            let arrow = match &grid.sort {
                Some((path, false)) if *path == column.path => " ▲",
                Some((path, true)) if *path == column.path => " ▼",
                _ => "",
            };
            let pin = if column.pinned { "📌 " } else { "" };
            let label = if column.path.is_empty() { "value" } else { column.path.as_str() };
            let (rect, _) = ui.allocate_exact_size(egui::vec2(column.width, height), egui::Sense::hover());
            let mut header_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect).id_salt(("bulk_header", index)));
            header_ui.set_clip_rect(rect);
            let response = header_ui
                .add_sized(rect.size() - egui::vec2(4.0, 0.0), egui::Button::new(format!("{pin}{label}{arrow}")))
                .on_hover_text("Sort by this column (click again to reverse, a third time to clear); right-click to pin or hide");
            if response.clicked() {
                grid.sort = match grid.sort.take() {
                    Some((path, false)) if path == column.path => Some((path, true)),
                    Some((path, true)) if path == column.path => None,
                    _ => Some((column.path.clone(), false)),
                };
            }
            let path = column.path.clone();
            response.context_menu(|ui| {
                let pinned = grid.columns.iter().any(|c| c.path == path && c.pinned);
                if ui.button(if pinned { "Unpin" } else { "Pin" }).clicked() {
                    if let Some(c) = grid.columns.iter_mut().find(|c| c.path == path) {
                        c.pinned = !c.pinned;
                    }
                    ui.close();
                }
                if ui.button("Hide").clicked() {
                    grid.columns.retain(|c| c.path != path);
                    ui.close();
                }
            });
            // The resize handle: the header's right edge.
            let handle = egui::Rect::from_min_max(egui::pos2(rect.right() - 4.0, rect.top()), rect.right_bottom());
            let drag = ui.interact(handle, egui::Id::new(("bulk_resize", index)), egui::Sense::drag());
            drag.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, format!("Resize {label} (Alt+Left or Alt+Right in the grid)")));
            if drag.hovered() || drag.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
            if drag.dragged()
                && let Some(c) = grid.columns.iter_mut().find(|c| c.path == path)
            {
                c.width = (c.width + drag.drag_delta().x).clamp(MIN_WIDTH, MAX_WIDTH);
            }
        }
        row_cell
    })
    .inner
}

fn draw_column_picker(ui: &mut egui::Ui, panel: &mut BulkEdit, grid: &mut Grid) {
    egui::CollapsingHeader::new(format!(
        "Columns ({} of {})",
        grid.columns.len(),
        grid.available.len()
    ))
    .id_salt("bulk_edit_columns")
    .show(ui, |ui| {
        let search = ui.add(egui::TextEdit::singleline(&mut panel.picker_search).hint_text("Search fields"));
        crate::a11y_audit::name(search, egui::WidgetType::TextEdit, "Search fields");
        let query = panel.picker_search.trim().to_lowercase();
        egui::ScrollArea::vertical()
            .id_salt("bulk_edit_column_list")
            .max_height(ui.spacing().interact_size.y * 8.0)
            .show(ui, |ui| {
                for path in grid.available.clone() {
                    let label = if path.is_empty() { "value" } else { path.as_str() };
                    if !query.is_empty() && !label.to_lowercase().contains(&query) {
                        continue;
                    }
                    ui.horizontal(|ui| {
                        let mut shown = grid.columns.iter().any(|c| c.path == path);
                        if ui.checkbox(&mut shown, label).changed() {
                            if shown {
                                grid.columns.push(Column {
                                    path: path.clone(),
                                    width: DEFAULT_WIDTH,
                                    pinned: false,
                                });
                            } else {
                                grid.columns.retain(|c| c.path != path);
                            }
                        }
                        if let Some(column) = grid.columns.iter_mut().find(|c| c.path == path) {
                            let mut pinned = column.pinned;
                            let pin = ui.checkbox(&mut pinned, "Pin");
                            if crate::a11y_audit::name(
                                pin,
                                egui::WidgetType::Checkbox,
                                &format!("Pin {label}"),
                            )
                            .changed()
                            {
                                column.pinned = pinned;
                            }
                        }
                    });
                }
            });
    });
}

fn handle_keys(
    ui: &mut egui::Ui,
    world: &mut World,
    panel: &mut BulkEdit,
    grid: &mut Grid,
    rows: usize,
    shown: &[Column],
) {
    let frame = ui.ctx().cumulative_frame_nr();
    if panel.keys_frame == Some(frame) || panel.edit.is_some() {
        return;
    }
    panel.keys_frame = Some(frame);
    let events = ui.input(|i| i.events.clone());
    for event in events {
        let range = grid.range();
        match event {
            egui::Event::Copy => match copy_from(world, grid, range) {
                Ok(text) => ui.ctx().copy_text(text),
                Err(e) => panel.status = e.message,
            },
            egui::Event::Paste(text) => match paste_into(world, grid, &text, range.row, range.column) {
                Ok(report) => {
                    panel.status = format!("Pasted {} cells ({} changed)", report.set, report.changed)
                }
                Err(e) => panel.status = e.message,
            },
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => match grid_key(key, modifiers) {
                Some(GridKey::Edit) if rows > 0 => {
                    let registry = registry_of(world).ok();
                    let text = registry
                        .and_then(|r| {
                            let order = &grid.view.order;
                            let data_row = *order.get(grid.cursor.0)?;
                            read_cell_text(world, &r.read(), grid, data_row, &shown[grid.cursor.1].path)
                        })
                        .unwrap_or_default();
                    panel.edit = Some(CellEdit {
                        row: grid.cursor.0,
                        column: grid.cursor.1,
                        text,
                        focused: false,
                    });
                    break;
                }
                Some(GridKey::FillDown) => match fill_into(world, grid, range) {
                    Ok(report) => panel.status = format!("Filled {} cells", report.changed),
                    Err(e) => panel.status = e.message,
                },
                Some(GridKey::Width(by)) => {
                    let path = &shown[grid.cursor.1].path;
                    if let Some(c) = grid.columns.iter_mut().find(|c| c.path == *path) {
                        c.width = (c.width + by).clamp(MIN_WIDTH, MAX_WIDTH);
                    }
                }
                Some(GridKey::Leave) => {
                    ui.memory_mut(|m| m.surrender_focus(grid_focus_id()));
                }
                Some(key) => move_cursor(grid, key, rows, shown.len()),
                None => {}
            },
            _ => {}
        }
    }
}

/// Whether a grid is open in the Bulk Edit panel.
#[must_use]
pub fn grid_open(world: &World) -> bool {
    world.get_resource::<BulkEdit>().is_some_and(|b| b.grid.is_some())
}

/// Opens the grid on `entities` (the Outliner's **Edit in grid**). Returns the status line.
pub fn edit_entities_in_grid(world: &mut World, entities: &[Entity]) -> String {
    match open_entities(world, entities, None, None) {
        Ok(()) => world
            .get_resource::<BulkEdit>()
            .map(|b| b.status.clone())
            .unwrap_or_default(),
        Err(e) => {
            let message = e.message.clone();
            world.get_resource_or_insert_with(BulkEdit::default).status = e.message;
            message
        }
    }
}

/// Opens the grid on the data assets of `rel`'s type in its folder (the Content Browser's **Edit in
/// grid**). Returns the status line.
pub fn edit_assets_in_grid(world: &mut World, root: &Path, rel: &str) -> String {
    let result = sibling_assets(root, rel).and_then(|paths| open_assets_at(world, root, &paths, None, None));
    match result {
        Ok(()) => world
            .get_resource::<BulkEdit>()
            .map(|b| b.status.clone())
            .unwrap_or_default(),
        Err(e) => e.message,
    }
}

/// What [`edit_selection_in_grid`] opened (P-145, ADR-8147).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionGrid {
    /// The `.data.ron` assets that became the grid's rows, in selection order.
    pub rows: Vec<String>,
    /// The selected files that are not data assets (no grid row for them).
    pub skipped: Vec<String>,
}

/// Opens the Bulk Edit grid on the data assets among `paths` (the Content Browser selection,
/// project-relative): the selection menu's **Edit in grid**. Files that are not `.data.ron` are
/// skipped and listed; the data assets must share one type.
///
/// # Errors
/// `bulk_edit_no_rows` (no data asset selected), `bulk_edit_mixed_types`, `bulk_edit_unsaved` and
/// the `data_asset_*` codes of reading a file.
pub fn edit_selection_in_grid(
    world: &mut World,
    root: &Path,
    paths: &[String],
) -> Result<SelectionGrid, PanelError> {
    let suffix = format!(".{DATA_ASSET_EXTENSION}");
    let mut out = SelectionGrid::default();
    for path in paths {
        let rel = path.replace('\\', "/");
        let rel = if rel.starts_with("assets/") {
            rel
        } else {
            format!("assets/{rel}")
        };
        if rel.ends_with(&suffix) {
            if !out.rows.contains(&rel) {
                out.rows.push(rel);
            }
        } else {
            out.skipped.push(path.clone());
        }
    }
    if out.rows.is_empty() {
        return Err(err(
            "bulk_edit_no_rows",
            "no data asset (.data.ron) is selected; Edit in grid edits data assets",
        ));
    }
    open_assets_at(world, root, &out.rows, None, None)?;
    Ok(out)
}

/// `editor.bulk_edit`.
fn bulk_edit_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or_else(|| err("bulk_edit_no_project", "no project is open").rpc(HINT))?;
    let paths = match strings_param(params, "assets")? {
        Some(paths) => paths,
        None => world
            .get_resource::<crate::content_actions::ContentSelection>()
            .map(|s| s.paths().to_vec())
            .unwrap_or_default(),
    };
    if params.get("discard").and_then(Value::as_bool) == Some(true)
        && let Some(mut panel) = world.get_resource_mut::<BulkEdit>()
    {
        panel.grid = None;
    }
    let opened = edit_selection_in_grid(world, &root, &paths).map_err(|e| e.rpc(HINT))?;
    let mut value = state_json(world, 0, DEFAULT_LIMIT, None).map_err(|e| e.rpc(HINT))?;
    if let Some(object) = value.as_object_mut() {
        object.insert("skipped".into(), json!(opened.skipped));
    }
    Ok(value)
}

/// The selection's Bulk Edit agent method (`editor.bulk_edit`, P-145), part of [`methods`].
fn selection_methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.bulk_edit",
            tool: "bulk_edit",
            description: "The Content Browser selection menu's Edit in grid: open the Bulk Edit grid with the \
                selected data assets as rows (`assets`, default the Content Browser selection). Files that are \
                not .data.ron are skipped and listed in `skipped`; the data assets must share one type \
                (bulk_edit_mixed_types). Refused with bulk_edit_unsaved while the open grid has unsaved asset \
                edits, unless discard: true.",
            params: &[
                ParamSchema {
                    name: "assets",
                    kind: "array",
                    required: false,
                    description: "Project-relative asset paths; default the Content Browser selection.",
                },
                ParamSchema {
                    name: "discard",
                    kind: "boolean",
                    required: false,
                    description: "Drop the open grid's unsaved asset edits first.",
                },
            ],
            result: "the editor.bulk_edit_state result plus skipped: [paths]",
            mutating: true,
            permission: Permission::Read,
        },
        bulk_edit_method as Handler,
    )]
}

#[cfg(test)]
mod tests;
