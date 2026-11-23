// SPDX-License-Identifier: MPL-2.0
//! Inspect: the data table of one node's output (spec 67 § 3). One tab per output pin, an item
//! picker, columns for built-ins and attributes (sortable, filterable by `attr op value`), paging
//! by [`PAGE_ROWS`]; a row selects its element and a viewport click selects the row.

use std::cmp::Ordering;

use regia_ecs::prelude::*;
use regia_procgen::data::{AttrColumn, Payload, PointSet};
use regia_procgen::world::{DebugOutputs, ProcGenState};
use regia_procgen::{DataItem, Table};

use super::debug_view::{PAGE_ROWS, debug_outputs};
use super::doc::DocError;

/// The inspect state (world resource).
#[derive(Resource, Clone, Debug, Default, PartialEq)]
pub struct ProcInspect {
    /// The inspected node.
    pub node: Option<u32>,
    /// Output pin index (tab).
    pub pin: usize,
    /// Item index within the pin's bundle.
    pub item: usize,
    /// Sort column and direction (`true` = descending).
    pub sort: Option<(String, bool)>,
    /// Filter expression `attr op value` (`op`: `<`, `<=`, `>`, `>=`, `==`, `!=`).
    pub filter: Option<String>,
    /// Page shown.
    pub page: usize,
    /// The selected element (point or row index in the item).
    pub selected: Option<usize>,
    /// The selected point's position (highlighted in the viewport).
    pub selected_pos: Option<[f32; 3]>,
    /// Tiled generators: the cell `(tier, i, j)` whose data is shown (spec 66).
    pub cell: Option<(u32, i32, i32)>,
    /// The hovered row's element (spec 67 § 3: row hover highlights the element in the viewport).
    /// View-only: no agent path, the parity allowlist row is `view_only`.
    pub hover: Option<usize>,
    /// The hovered element's position (the viewport marker).
    pub hover_pos: Option<[f32; 3]>,
}

impl ProcInspect {
    /// The selected point's position and element index.
    #[must_use]
    pub fn selected_point(&self) -> Option<([f32; 3], usize)> {
        self.selected_pos.zip(self.selected)
    }

    /// The hovered row's position and element index ([`set_hover`]).
    #[must_use]
    pub fn hovered_point(&self) -> Option<([f32; 3], usize)> {
        self.hover_pos.zip(self.hover)
    }
}

/// One table cell.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    /// A number.
    Num(f64),
    /// Text (names, assets, vectors written out).
    Text(String),
}

impl Cell {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Cell::Num(a), Cell::Num(b)) => a.total_cmp(b),
            (Cell::Text(a), Cell::Text(b)) => a.cmp(b),
            (Cell::Num(_), Cell::Text(_)) => Ordering::Less,
            (Cell::Text(_), Cell::Num(_)) => Ordering::Greater,
        }
    }

    /// The cell as shown.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Cell::Num(v) => {
                if v.fract() == 0.0 && v.abs() < 1e15 {
                    format!("{v:.0}")
                } else {
                    format!("{v:.4}")
                }
            }
            Cell::Text(t) => t.clone(),
        }
    }
}

/// The inspected item as a table.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DataTable {
    /// Output pins of the node (tabs).
    pub pins: usize,
    /// Items on the shown pin.
    pub items: usize,
    /// The item's kind (`points`, `table`, ...).
    pub kind: String,
    /// The item's tags.
    pub tags: Vec<String>,
    /// Column names.
    pub columns: Vec<String>,
    /// Rows in element order (`rows[k]` is element `k`).
    pub rows: Vec<Vec<Cell>>,
    /// Element indices after filtering and sorting (what the view shows, in order).
    pub order: Vec<usize>,
    /// The tiled cell the data came from `(tier, i, j)`; `None` for an untiled run (spec 66).
    pub cell: Option<(u32, i32, i32)>,
}

impl DataTable {
    /// The element indices on page `page`.
    #[must_use]
    pub fn page(&self, page: usize) -> &[usize] {
        let start = (page * PAGE_ROWS).min(self.order.len());
        let end = (start + PAGE_ROWS).min(self.order.len());
        &self.order[start..end]
    }

    /// Number of pages (at least one).
    #[must_use]
    pub fn pages(&self) -> usize {
        self.order.len().div_ceil(PAGE_ROWS).max(1)
    }

    fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }
}

fn column_cells(col: &AttrColumn, i: usize) -> Vec<Cell> {
    let f = |v: f64| Cell::Num(v);
    match col {
        AttrColumn::Bool(v) => vec![f(v.get(i).map_or(0.0, |b| f64::from(u8::from(*b))))],
        AttrColumn::Int(v) => vec![f(v.get(i).map_or(0.0, |x| *x as f64))],
        AttrColumn::Float(v) => vec![f(v.get(i).copied().unwrap_or(0.0))],
        AttrColumn::Vec2(v) => v
            .get(i)
            .map_or_else(|| vec![f(0.0); 2], |x| x.iter().map(|c| f(*c)).collect()),
        AttrColumn::Vec3(v) => v
            .get(i)
            .map_or_else(|| vec![f(0.0); 3], |x| x.iter().map(|c| f(*c)).collect()),
        AttrColumn::Vec4(v) | AttrColumn::Quat(v) => v
            .get(i)
            .map_or_else(|| vec![f(0.0); 4], |x| x.iter().map(|c| f(*c)).collect()),
        AttrColumn::Transform(v) => vec![Cell::Text(v.get(i).map(|x| format!("{x:?}")).unwrap_or_default())],
        AttrColumn::Name(v) | AttrColumn::Asset(v) => vec![Cell::Text(v.get(i).cloned().unwrap_or_default())],
    }
}

fn column_names(name: &str, col: &AttrColumn) -> Vec<String> {
    let parts: &[&str] = match col {
        AttrColumn::Vec2(_) => &["x", "y"],
        AttrColumn::Vec3(_) => &["x", "y", "z"],
        AttrColumn::Vec4(_) | AttrColumn::Quat(_) => &["x", "y", "z", "w"],
        _ => &[],
    };
    if parts.is_empty() {
        vec![name.to_string()]
    } else {
        parts.iter().map(|p| format!("{name}.{p}")).collect()
    }
}

fn points_table(set: &PointSet) -> (Vec<String>, Vec<Vec<Cell>>) {
    let mut columns: Vec<String> = [
        "index",
        "position.x",
        "position.y",
        "position.z",
        "rotation.x",
        "rotation.y",
        "rotation.z",
        "rotation.w",
        "scale.x",
        "scale.y",
        "scale.z",
        "density",
        "seed",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    for (name, col) in &set.attrs {
        columns.extend(column_names(name, col));
    }
    let rows = (0..set.len())
        .map(|i| {
            let mut row = vec![Cell::Num(i as f64)];
            row.extend(set.position[i].iter().map(|v| Cell::Num(f64::from(*v))));
            row.extend(set.rotation[i].iter().map(|v| Cell::Num(f64::from(*v))));
            row.extend(set.scale[i].iter().map(|v| Cell::Num(f64::from(*v))));
            row.push(Cell::Num(f64::from(set.density[i])));
            row.push(Cell::Num(f64::from(set.seed[i])));
            for col in set.attrs.values() {
                row.extend(column_cells(col, i));
            }
            row
        })
        .collect();
    (columns, rows)
}

fn table_table(t: &Table) -> (Vec<String>, Vec<Vec<Cell>>) {
    let mut columns = vec!["index".to_string()];
    for (name, col) in &t.columns {
        columns.extend(column_names(name, col));
    }
    let rows = (0..t.rows)
        .map(|i| {
            let mut row = vec![Cell::Num(i as f64)];
            for (_, col) in &t.columns {
                row.extend(column_cells(col, i));
            }
            row
        })
        .collect();
    (columns, rows)
}

fn item_table(item: &DataItem) -> (String, Vec<String>, Vec<Vec<Cell>>) {
    let kind = item.payload.kind().name().to_string();
    let (columns, rows) = match &item.payload {
        Payload::Points(set) => points_table(set),
        Payload::Table(t) => table_table(t),
        _ => (vec!["index".into()], Vec::new()),
    };
    (kind, columns, rows)
}

/// A parsed filter `attr op value`.
#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    /// Column name.
    pub column: String,
    /// Operator.
    pub op: String,
    /// Value (a number, else text).
    pub value: Cell,
}

/// Parses `attr op value`.
///
/// # Errors
///
/// `procgen_filter_invalid` naming what is wrong.
pub fn parse_filter(text: &str) -> Result<Filter, DocError> {
    let bad = |m: &str| {
        DocError::new(
            "procgen_filter_invalid",
            format!("{m} (expected `attr op value`)"),
        )
    };
    let ops = ["<=", ">=", "==", "!=", "<", ">"];
    let (pos, op) = ops
        .iter()
        .filter_map(|op| text.find(op).map(|p| (p, *op)))
        .min_by_key(|(p, op)| (*p, std::cmp::Reverse(op.len())))
        .ok_or_else(|| bad("no operator"))?;
    let column = text[..pos].trim().to_string();
    let raw = text[pos + op.len()..].trim();
    if column.is_empty() || raw.is_empty() {
        return Err(bad("missing column or value"));
    }
    let value = raw
        .parse::<f64>()
        .map_or_else(|_| Cell::Text(raw.trim_matches('"').to_string()), Cell::Num);
    Ok(Filter {
        column,
        op: op.to_string(),
        value,
    })
}

fn keeps(cell: &Cell, f: &Filter) -> bool {
    let o = cell.cmp(&f.value);
    match f.op.as_str() {
        "<" => o == Ordering::Less,
        "<=" => o != Ordering::Greater,
        ">" => o == Ordering::Greater,
        ">=" => o != Ordering::Less,
        "==" => o == Ordering::Equal,
        _ => o != Ordering::Equal,
    }
}

/// The cells of the preview target's last tiled run that kept debug data, in key order
/// `(tier, i, j)` (spec 66 § 2: each cell holds its own debug outputs). Empty for an untiled
/// target (or before the first run finishes).
#[must_use]
pub fn inspect_cells(world: &World) -> Vec<(u32, i32, i32)> {
    let Some(target) = (world.get_resource::<super::debug_view::ProcPreview>()).and_then(|p| p.target) else {
        return Vec::new();
    };
    world
        .get::<ProcGenState>(target)
        .and_then(|s| s.tiled.as_ref())
        .map(|tiled| {
            tiled
                .cells
                .values()
                .filter(|c| c.debug.is_some())
                .map(|c| (u32::from(c.key.tier), c.key.cell.x as i32, c.key.cell.z as i32))
                .collect()
        })
        .unwrap_or_default()
}

/// What [`inspect_debug`] answers: the debug outputs and the cell `(tier, i, j)` they came from
/// (`None` for whole-run data).
type InspectDebug = (std::sync::Arc<DebugOutputs>, Option<(u32, i32, i32)>);

/// The debug outputs the Inspect table reads and the cell they came from: the isolate's while
/// isolating, else the untiled target's whole-run data (`cell: None`); a tiled target's run keeps
/// no whole-run debug data, so the picked cell's is read instead (spec 66 § 2) — [`ProcInspect`]'s
/// pick when it still exists, else the first cell with data (in key order).
fn inspect_debug(world: &World) -> Option<InspectDebug> {
    if let Some(whole) = debug_outputs(world) {
        return Some((whole, None));
    }
    let pick =
        |key: regia_procgen::tiles::TileKey| (u32::from(key.tier), key.cell.x as i32, key.cell.z as i32);
    let inspect = world.get_resource::<ProcInspect>().cloned().unwrap_or_default();
    let target = world
        .get_resource::<super::debug_view::ProcPreview>()
        .and_then(|p| p.target)?;
    let tiled = world.get::<ProcGenState>(target)?.tiled.clone()?;
    let cells: Vec<regia_procgen::tiles::TileKey> = tiled
        .cells
        .values()
        .filter(|c| c.debug.is_some())
        .map(|c| c.key)
        .collect();
    // The picked cell when it still exists, else the first cell with data.
    let key = inspect
        .cell
        .and_then(|want| cells.iter().copied().find(|k| pick(*k) == want))
        .or_else(|| cells.first().copied())?;
    let debug = tiled.cells.get(&key)?.debug.clone()?;
    Some((debug, Some(pick(key))))
}

/// Builds the data table of the inspected node from the preview's debug data.
///
/// # Errors
///
/// `procgen_no_inspect` (no node inspected), `procgen_no_debug_data` (no preview run kept data),
/// `procgen_debug_dropped` (the node's data was dropped by the cap), `procgen_filter_invalid`,
/// `procgen_unknown_column`.
#[tracing::instrument(target = "regia_editor", skip_all, name = "procgen_table")]
pub fn build_table(world: &World) -> Result<DataTable, DocError> {
    let inspect = world.get_resource::<ProcInspect>().cloned().unwrap_or_default();
    let node = inspect
        .node
        .ok_or_else(|| DocError::new("procgen_no_inspect", "no node is inspected"))?;
    let Some((debug, cell)) = inspect_debug(world) else {
        return Err(DocError::new(
            "procgen_no_debug_data",
            "no debug data: bind a preview target and let it run",
        ));
    };
    let Some(bundles) = debug.get(&node) else {
        let dropped = world
            .get_resource::<super::ProcPreview>()
            .is_some_and(|p| p.dropped_debug.contains(&node));
        return Err(if dropped {
            DocError::new(
                "procgen_debug_dropped",
                format!("node {node}'s data was dropped by the 256 MiB debug cap"),
            )
        } else {
            DocError::new(
                "procgen_no_debug_data",
                format!("node {node} has no output in the last run"),
            )
        });
    };
    let mut table = DataTable {
        pins: bundles.len(),
        cell,
        ..DataTable::default()
    };
    let Some(bundle) = bundles.get(inspect.pin) else {
        return Ok(table);
    };
    table.items = bundle.0.len();
    let Some(item) = bundle.0.get(inspect.item) else {
        return Ok(table);
    };
    let (kind, columns, rows) = item_table(item);
    table.kind = kind;
    table.tags = item.tags.iter().cloned().collect();
    table.columns = columns;
    table.rows = rows;
    let mut order: Vec<usize> = (0..table.rows.len()).collect();
    if let Some(text) = inspect.filter.as_deref().filter(|t| !t.trim().is_empty()) {
        let f = parse_filter(text)?;
        let c = table
            .column(&f.column)
            .ok_or_else(|| DocError::new("procgen_unknown_column", format!("no column `{}`", f.column)))?;
        order.retain(|&r| keeps(&table.rows[r][c], &f));
    }
    if let Some((name, descending)) = &inspect.sort {
        let c = table
            .column(name)
            .ok_or_else(|| DocError::new("procgen_unknown_column", format!("no column `{name}`")))?;
        // `sort_by` is stable: equal keys keep element order (spec 67 § 7 test 7).
        order.sort_by(|a, b| {
            let o = table.rows[*a][c].cmp(&table.rows[*b][c]);
            if *descending { o.reverse() } else { o }
        });
    }
    table.order = order;
    Ok(table)
}

/// A data-table control of the panel (spec 67 § 3): a column header click, the filter field, the
/// pager, the pin tabs, the item picker and a row click. The agent's `editor.procgen_table` takes
/// the same values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableControl {
    /// A header click on a column: ascending, then descending, then unsorted.
    ToggleSort(String),
    /// A filter `attr op value`; empty clears. A refused filter leaves the current one.
    Filter(String),
    /// Show this page (clamped to the last page).
    Page(usize),
    /// Show this output pin (its first item, first page).
    Pin(usize),
    /// Show this item of the pin (first page).
    Item(usize),
    /// Select the element in this row of the current page.
    SelectRow(usize),
    /// Show this cell of a tiled generator `(tier, i, j)` (spec 67 § 3; first page, nothing
    /// selected). `editor.procgen_table { cell }` is the agent path.
    Cell(u32, i32, i32),
}

/// The next sort state after a header click on `column`.
#[must_use]
pub fn next_sort(current: Option<&(String, bool)>, column: &str) -> Option<(String, bool)> {
    match current {
        Some((c, false)) if c == column => Some((column.to_string(), true)),
        Some((c, true)) if c == column => None,
        _ => Some((column.to_string(), false)),
    }
}

/// Applies one table control to the inspect state. A filter is checked against the current
/// table first, so a bad one never replaces a good one; a page is clamped to the table.
///
/// # Errors
///
/// `procgen_no_inspect`; for `Filter`, `procgen_filter_invalid` and `procgen_unknown_column`; for
/// `SelectRow`, as [`select_row`]; for `ToggleSort`, `procgen_unknown_column`; for `Cell`,
/// `procgen_unknown_cell` (the run has no such cell).
pub fn apply_table_control(world: &mut World, control: TableControl) -> Result<(), DocError> {
    let current = world.get_resource::<ProcInspect>().cloned().unwrap_or_default();
    if current.node.is_none() {
        return Err(DocError::new("procgen_no_inspect", "no node is inspected"));
    }
    let mut next = current.clone();
    match control {
        TableControl::ToggleSort(column) => next.sort = next_sort(current.sort.as_ref(), &column),
        TableControl::Filter(text) => {
            next.filter = (!text.trim().is_empty()).then_some(text);
            next.page = 0;
        }
        TableControl::Page(page) => next.page = page,
        TableControl::Pin(pin) => {
            next.pin = pin;
            next.item = 0;
            next.page = 0;
            next.selected = None;
            next.selected_pos = None;
            next.hover = None;
            next.hover_pos = None;
        }
        TableControl::Item(item) => {
            next.item = item;
            next.page = 0;
            next.selected = None;
            next.selected_pos = None;
            next.hover = None;
            next.hover_pos = None;
        }
        TableControl::SelectRow(row) => {
            let start = current.page.saturating_mul(PAGE_ROWS);
            return select_row(world, start.saturating_add(row)).map(|_| ());
        }
        TableControl::Cell(tier, i, j) => {
            let want = (tier, i, j);
            if !inspect_cells(world).contains(&want) {
                return Err(DocError::new(
                    "procgen_unknown_cell",
                    format!(
                        "the tiled run has no cell ({tier}, {i}, {j}); it has {}",
                        inspect_cells(world).len()
                    ),
                ));
            }
            next.cell = Some(want);
            next.page = 0;
            next.selected = None;
            next.selected_pos = None;
            next.hover = None;
            next.hover_pos = None;
        }
    }
    // Check the new view against the table before keeping it.
    world.insert_resource(next);
    match build_table(world) {
        Ok(t) => {
            let mut ins = world.resource_mut::<ProcInspect>();
            ins.page = ins.page.min(t.pages() - 1);
            Ok(())
        }
        Err(e)
            if matches!(
                e.code.as_str(),
                "procgen_filter_invalid" | "procgen_unknown_column"
            ) =>
        {
            world.insert_resource(current);
            Err(e)
        }
        // No debug data yet: keep the view (it applies when data arrives).
        Err(_) => Ok(()),
    }
}

/// Selects the element shown in view row `row` (an index into the filtered, sorted order); returns
/// the element index.
///
/// # Errors
///
/// As [`build_table`], plus `procgen_row_out_of_range`.
pub fn select_row(world: &mut World, row: usize) -> Result<usize, DocError> {
    let table = build_table(world)?;
    let element = *table.order.get(row).ok_or_else(|| {
        DocError::new(
            "procgen_row_out_of_range",
            format!("row {row} of {}", table.order.len()),
        )
    })?;
    let pos = position_of(&table, element);
    let mut inspect = world.get_resource_or_insert_with(ProcInspect::default);
    inspect.selected = Some(element);
    inspect.selected_pos = pos;
    Ok(element)
}

/// Sets the hovered row's element (spec 67 § 3: the row hover highlights the element in the
/// viewport) or clears it (`None`) when the pointer left the table. The position is resolved
/// against the current table only when the element changes, so the panel's per-frame call is one
/// comparison while the hover holds. View-only: no agent path (the allowlist row is `view_only`).
pub fn set_hover(world: &mut World, element: Option<usize>) {
    let current = world
        .get_resource::<ProcInspect>()
        .map_or((None, None), |i| (i.hover, i.hover_pos));
    let held = current.0 == element && (element.is_none() || current.1.is_some());
    if held {
        return;
    }
    let pos = element.and_then(|e| build_table(world).ok().and_then(|t| position_of(&t, e)));
    let mut inspect = world.get_resource_or_insert_with(ProcInspect::default);
    inspect.hover = element;
    inspect.hover_pos = pos;
}

fn position_of(table: &DataTable, element: usize) -> Option<[f32; 3]> {
    let (x, y, z) = (
        table.column("position.x")?,
        table.column("position.y")?,
        table.column("position.z")?,
    );
    let row = table.rows.get(element)?;
    let num = |c: usize| match row.get(c) {
        Some(Cell::Num(v)) => *v as f32,
        _ => 0.0,
    };
    Some([num(x), num(y), num(z)])
}

/// A viewport click near `position` selects the nearest point's row (within `radius_m`); returns
/// the view row.
///
/// # Errors
///
/// As [`build_table`], plus `procgen_no_point` when nothing is within the radius or the point is
/// filtered out.
pub fn select_at(world: &mut World, position: [f32; 3], radius_m: f32) -> Result<usize, DocError> {
    let table = build_table(world)?;
    let mut best: Option<(f32, usize)> = None;
    for element in 0..table.rows.len() {
        if let Some(p) = position_of(&table, element) {
            let d2 = (0..3).map(|k| (p[k] - position[k]).powi(2)).sum::<f32>();
            if d2 <= radius_m * radius_m && best.is_none_or(|(b, _)| d2 < b) {
                best = Some((d2, element));
            }
        }
    }
    let (_, element) = best.ok_or_else(|| DocError::new("procgen_no_point", "no point under the cursor"))?;
    let row = table
        .order
        .iter()
        .position(|e| *e == element)
        .ok_or_else(|| DocError::new("procgen_no_point", "the point is filtered out of the table"))?;
    select_row(world, row)?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_parse() {
        let f = parse_filter("density >= 0.5").expect("parse");
        assert_eq!((f.column.as_str(), f.op.as_str()), ("density", ">="));
        assert_eq!(f.value, Cell::Num(0.5));
        let g = parse_filter("mesh == \"rock.glb\"").expect("parse");
        assert_eq!(g.value, Cell::Text("rock.glb".into()));
        assert!(parse_filter("density").is_err());
        assert!(parse_filter(">= 2").is_err());
    }

    // ---- The cell picker (spec 67 § 3): a tiled generator's Inspect shows the picked cell ----
    // and the row hover (spec 67 § 3): the hovered row's element is highlighted in the viewport.

    use regia_app::prelude::*;
    use regia_core::id::Guid;
    use regia_core::math::Vec3;
    use regia_procgen::graph::ParamDecl;
    use regia_procgen::tiles::TileSettings;
    use regia_procgen::world::{
        ProcGenPlugin, ProcGenSettings, ProcGenState, ProcGenerator, ProcGraphSource,
    };
    use regia_procgen::{AttrType, Edge, NodeEntry, ProcGraph, SettingValue};
    use regia_scene::components::EntityGuid;
    use regia_scene::prelude::Transform;

    fn node(ty: &str, at: f64, settings: &[(&str, &str)]) -> NodeEntry {
        let mut n = NodeEntry::new(ty);
        n.at = (at, 0.0);
        for (k, v) in settings {
            n = n.with_ron(k, v);
        }
        n
    }

    /// in.self → sample.region (a 4 m world lattice) → out.points.
    fn grid_graph() -> ProcGraph {
        let mut g = ProcGraph::default();
        g.params.push(ParamDecl {
            name: "spacing".into(),
            ty: AttrType::Float,
            default: SettingValue::Float(4.0),
            min: Some(0.5),
            max: Some(64.0),
            doc: String::new(),
        });
        g.nodes.insert(1, node("in.self", 0.0, &[]));
        g.nodes.insert(
            2,
            node("sample.region", 200.0, &[("spacing_m", "Bind(\"spacing\")")]),
        );
        g.nodes.insert(4, node("out.points", 600.0, &[]));
        g.edges = vec![Edge::new(1, "out", 2, "region"), Edge::new(2, "out", 4, "in")];
        g
    }

    /// The headless harness of `tests/procgen_editor.rs`, inlined: in-memory graphs, inline runs.
    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(ProcGenSettings {
            inline: true,
            source: ProcGraphSource::Memory(
                [("g.pgraph.ron".to_string(), grid_graph().to_ron())]
                    .into_iter()
                    .collect(),
            ),
            ..ProcGenSettings::default()
        });
        app.add_plugins(ProcGenPlugin);
        app.init_resource::<crate::command::UndoStack>();
        crate::procgen::install(app.world_mut());
        app.finish_plugins();
        app.update();
        app
    }

    fn frame(app: &mut App) {
        crate::procgen::tick(app.world_mut());
        app.update();
    }

    /// A tiled generator over a 16 m square with 8 m cells, previewed to Ready with debug data.
    fn tiled_preview(app: &mut App) -> Entity {
        let e = app
            .world_mut()
            .spawn((
                ProcGenerator {
                    bounds: regia_procgen::world::ProcBounds::Box {
                        half_extents: Vec3::new(8.0, 2.0, 8.0),
                    },
                    tiles: Some(TileSettings {
                        cell_m: 8.0,
                        ..TileSettings::default()
                    }),
                    ..ProcGenerator::new("g.pgraph.ron")
                },
                EntityGuid(Guid(0x00ab_cdef_0123_4567_89ab_cdef_0123_4567)),
                Transform::IDENTITY,
            ))
            .id();
        super::super::doc::open(app.world_mut(), "g.pgraph.ron").expect("open");
        super::super::debug_view::set_preview(app.world_mut(), Some(e)).expect("preview");
        for _ in 0..16 {
            frame(app);
        }
        let st = app.world().get::<ProcGenState>(e).expect("state");
        assert_eq!(
            (st.state, st.tiled.is_some()),
            (regia_procgen::world::ProcGenRunState::Ready, true),
            "the preview run finished tiled"
        );
        let every_cell_kept_debug = app
            .world()
            .get::<ProcGenState>(e)
            .and_then(|s| s.tiled.as_ref())
            .expect("tiled run")
            .cells
            .values()
            .all(|c| c.debug.is_some());
        assert!(every_cell_kept_debug, "every cell kept its debug data");
        e
    }

    /// The picked cell's rows replace the Inspect table (spec 67 § 3: a cell picker (tier, i, j)
    /// chooses which cell's data to show).
    #[test]
    fn picking_a_cell_changes_the_inspect_rows_to_that_cells() {
        let mut app = app();
        let e = tiled_preview(&mut app);
        let _ = e;
        *app.world_mut().resource_mut::<ProcInspect>() = ProcInspect {
            node: Some(2),
            ..ProcInspect::default()
        };
        // The run's cells, in key order: the tiler takes every 8 m cell touching the closed 16 m
        // square (`regia_procgen::tiles::cells_in_rect`), so the cells on its +8 m edges are in
        // the run too: the nine (i, j) in {-1, 0, 1}.
        let cells = inspect_cells(app.world());
        assert_eq!(
            cells,
            [
                (0, -1, -1),
                (0, -1, 0),
                (0, -1, 1),
                (0, 0, -1),
                (0, 0, 0),
                (0, 0, 1),
                (0, 1, -1),
                (0, 1, 0),
                (0, 1, 1)
            ],
            "key order"
        );
        // With no cell picked, the first cell's data shows.
        let first = build_table(app.world()).expect("the first cell's table");
        // A cell's debug data is its own working region: the 4 m world lattice (points at
        // x, z = ..., -6, -2, 2, 6, ...) over the cell's 8 m square and the run's apron, so 3 x 3
        // points for a cell inside the square.
        assert_eq!(
            first.rows.len(),
            9,
            "the 4 m lattice over one 8 m cell and its apron"
        );
        let xs = |t: &DataTable| {
            let mut v: Vec<f64> = t
                .order
                .iter()
                .map(|&e| match t.rows[e][1] {
                    Cell::Num(x) => x,
                    Cell::Text(_) => f64::NAN,
                })
                .collect();
            v.sort_by(f64::total_cmp);
            v
        };
        let columns = |t: &DataTable| {
            let mut v = xs(t);
            v.dedup();
            v
        };
        assert_eq!(
            columns(&first),
            [-6.0, -2.0, 2.0],
            "cell (-1, -1) covers the west of the square (x -8..0 plus the apron)"
        );
        // Picking (0, 0) changes the rows to that cell's (the east of the square).
        apply_table_control(app.world_mut(), TableControl::Cell(0, 0, 0)).expect("pick");
        let picked = build_table(app.world()).expect("the picked cell's table");
        assert_eq!(picked.rows.len(), 9);
        assert_eq!(
            columns(&picked),
            [-2.0, 2.0, 6.0],
            "cell (0, 0) covers the east of the square (x 0..8 plus the apron)"
        );
        assert_ne!(xs(&picked), xs(&first), "the rows changed with the pick");
        assert_eq!(app.world().resource::<ProcInspect>().page, 0);
        assert_eq!(picked.cell, Some((0, 0, 0)), "the table names its cell");
        // A cell outside the run refuses the control and keeps the view.
        let err =
            apply_table_control(app.world_mut(), TableControl::Cell(0, 9, 9)).expect_err("no such cell");
        assert_eq!(err.code, "procgen_unknown_cell");
        assert_eq!(build_table(app.world()).expect("kept").cell, Some((0, 0, 0)));
    }

    /// The hovered row's element and position feed the viewport marker, and leaving the table
    /// clears them (spec 67 § 3; view-only).
    #[test]
    fn a_hovered_row_reports_the_element_and_leaving_clears_it() {
        let mut app = app();
        tiled_preview(&mut app);
        *app.world_mut().resource_mut::<ProcInspect>() = ProcInspect {
            node: Some(2),
            ..ProcInspect::default()
        };
        let table = build_table(app.world()).expect("table");
        let at = |element: usize| -> [f32; 3] {
            let col = |name: &str| table.column(name).expect(name);
            let num = |c: usize| match table.rows[element][c] {
                Cell::Num(v) => v as f32,
                Cell::Text(_) => 0.0,
            };
            [
                num(col("position.x")),
                num(col("position.y")),
                num(col("position.z")),
            ]
        };
        set_hover(app.world_mut(), Some(2));
        assert_eq!(
            app.world().resource::<ProcInspect>().hovered_point(),
            Some((at(2), 2)),
            "the hovered element's position feeds the viewport marker"
        );
        set_hover(app.world_mut(), None);
        assert_eq!(app.world().resource::<ProcInspect>().hovered_point(), None);
    }
}
