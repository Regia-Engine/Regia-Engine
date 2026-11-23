// SPDX-License-Identifier: MPL-2.0
//! Drawn widgets over the ADR-1842 models (ADR-273 W10/W11, ADR-2045): a [`TreeView`] with
//! disclosure chevrons and click handling, a [`DataGridView`] with clickable sort headers and
//! column cells, a drawn color picker ([`ColorPickerView`]: saturation/value square, hue bar,
//! alpha bar, swatch and hex label) and a [`Separator`].
//!
//! The tree and the grid ride on the virtualised list (`crate::list`): only visible rows exist,
//! rows are [`CustomEntries`] filled from [`ListEntryBind`], so a tree of 10 000 nodes still
//! costs one entity per visible row. Every state change goes through the models
//! ([`UiTree`], [`DataGrid`], [`ColorPicker`]), so pointer input, the agent methods and the tests
//! share one code path. Nothing here runs for apps that spawn none of these widgets.

use regia_app::prelude::*;

use crate::color::UiColor;
use crate::color_picker::ColorPicker;
use crate::interaction::{UiEvent, UiEventKind, UiPointer};
use crate::list::{CustomEntries, ListEntryBind, ListRow, ListSelection, SelectionMode};
use crate::node::{
    Background, BackgroundKind, Border, Classes, ComputedNode, Interactable, NodeName, NodeStyle, UiNode,
};
use crate::scroll::{ListItem, ListItems, ListView, ScrollView};
use crate::style::{Align, Dimension, Edges, FlexDirection, Overflow, PositionType, Style};
use crate::text::{Text, TextContent, TextSpan, WrapMode};
use crate::theme::Themed;
use crate::tree::{DataGrid, TreeError, TreeSpec, UiTree};

/// Row height of the drawn tree and grid in UI px.
pub const ROW_HEIGHT: f32 = 28.0;

/// Chevron of a collapsed branch (U+25B6, in the bundled Inter).
pub const CHEVRON_CLOSED: &str = "\u{25B6}";
/// Chevron of an expanded branch (U+25BC, in the bundled Inter).
pub const CHEVRON_OPEN: &str = "\u{25BC}";
/// Sort arrow of an ascending grid column (U+25B2).
pub const SORT_ASC: &str = "\u{25B2}";
/// Sort arrow of a descending grid column (U+25BC).
pub const SORT_DESC: &str = "\u{25BC}";

/// A drawn tree view on a list entity that also carries a [`UiTree`] (spawn it with
/// [`spawn_tree_view`]). Rows show a chevron at `depth * indent` and the label after it; a click
/// on the chevron (or a double click anywhere on the row) expands or collapses, any click
/// selects.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct TreeView {
    /// Indent per depth level in UI px.
    pub indent: f32,
    /// Rows can be dragged onto other rows (ADR-2045): a drop sends [`TreeDropped`] and, with
    /// this on, moves the node in the [`UiTree`] (before, inside or after the target row by
    /// where the pointer is). Off by default.
    pub reorder: bool,
}

impl Default for TreeView {
    fn default() -> Self {
        Self {
            indent: 16.0,
            reorder: false,
        }
    }
}

/// The drag payload kind of drawn tree rows.
pub const TREE_DRAG_KIND: &str = "tree-node";

/// A tree row was dropped on another row of the same drawn tree (ADR-2045).
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeDropped {
    /// The tree's list entity.
    pub tree: Entity,
    /// The dragged node.
    pub source: usize,
    /// The node it was dropped on.
    pub target: usize,
    /// Where relative to the target.
    pub position: crate::tree::DropPosition,
    /// Whether the tree moved the node ([`TreeView::reorder`] and an allowed move).
    pub moved: bool,
}

/// The drop indicator node of a drawn tree (a bar before or after the hovered row, a box over
/// it for a drop inside), spawned the first time a drag hovers the tree.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeDropIndicator(pub Entity);

/// The child entities of one drawn tree row.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeRowParts {
    /// The chevron text node.
    pub chevron: Entity,
    /// The label text node.
    pub label: Entity,
}

/// A drawn data grid: a header row of sort buttons over a list entity carrying the [`DataGrid`]
/// (spawn it with [`spawn_data_grid`]). The component sits on the outer node.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataGridView {
    /// The list entity that holds the [`DataGrid`] and shows its rows.
    pub body: Entity,
    /// The header row.
    pub header: Entity,
}

/// A header cell of a [`DataGridView`]: clicking sorts by `column` (again flips the direction).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridHeaderCell {
    /// The grid's list entity.
    pub body: Entity,
    /// Column index.
    pub column: usize,
}

/// The cell entities of one drawn grid row.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct GridRowParts(pub Vec<Entity>);

/// Marker on a [`DataGrid`] list body: it is drawn by a [`DataGridView`] (the header lives on the
/// parent).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GridBody {
    /// The header row entity.
    pub header: Option<Entity>,
}

/// Which part of a drawn color picker a node is.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PickerPart {
    /// The saturation/value square.
    Square,
    /// The hue bar.
    Hue,
    /// The alpha bar.
    Alpha,
}

/// A drawn color picker over a [`ColorPicker`] on the same entity (spawn it with
/// [`spawn_color_picker`]). Pressing or dragging inside a part picks there.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorPickerView {
    /// The saturation/value square (its background is the pure hue).
    pub square: Entity,
    /// The square's white-to-transparent overlay.
    pub square_white: Entity,
    /// The hue bar.
    pub hue: Entity,
    /// The alpha bar (its background is the opaque color fading out).
    pub alpha: Entity,
    /// The square's marker ring.
    pub square_marker: Entity,
    /// The hue bar's marker.
    pub hue_marker: Entity,
    /// The alpha bar's marker.
    pub alpha_marker: Entity,
    /// The swatch showing the color.
    pub swatch: Entity,
    /// The `#RRGGBBAA` label.
    pub label: Entity,
}

/// Points back from a picker part to the entity holding the [`ColorPicker`].
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickerOwner(pub Entity);

/// A thin rule between groups of controls. Horizontal by default.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Separator {
    /// A vertical rule (in a row) instead of a horizontal one.
    pub vertical: bool,
}

fn text(label: &str) -> Text {
    Text {
        spans: vec![TextSpan::lit(label)],
        wrap: WrapMode::None,
        ..Default::default()
    }
}

/// Whether `t` already shows `label`, whatever color the theme gave it.
fn shows(t: &Text, label: &str) -> bool {
    matches!(t.spans.as_slice(), [TextSpan { content: TextContent::Lit(s), .. }] if s == label)
}

/// Writes `label` into a drawn widget's text and keeps its color: `apply_theme_text` wrote that
/// color, and only writes it again when the look changes.
fn relabel(t: &mut Text, label: &str) {
    let color = t.spans.first().map(|s| s.style.color);
    *t = text(label);
    if let (Some(color), Some(span)) = (color, t.spans.first_mut()) {
        span.style.color = color;
    }
}

fn abs(left: f32, top: f32, size: [Dimension; 2]) -> Style {
    Style {
        position: PositionType::Absolute,
        inset: Edges {
            left: Dimension::Points(left),
            top: Dimension::Points(top),
            right: Dimension::Auto,
            bottom: Dimension::Auto,
        },
        size,
        ..Default::default()
    }
}

/// Spawns a drawn tree view of `roots` under `parent`, `height` UI px tall. Errors when the
/// tree is too large or too deep ([`UiTree::new`]).
pub fn spawn_tree_view(
    commands: &mut Commands,
    parent: Entity,
    roots: &[TreeSpec],
    height: f32,
) -> Result<Entity, TreeError> {
    let tree = UiTree::new(roots)?;
    let entity = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Points(height)],
                overflow: [Overflow::Clip, Overflow::Clip],
                ..Default::default()
            }),
            ScrollView::vertical(),
            ListView {
                item_height: ROW_HEIGHT,
                ..Default::default()
            },
            ListItems::default(),
            ListSelection::new(SelectionMode::Single),
            CustomEntries,
            tree,
            TreeView::default(),
            Classes::one("tree-view"),
            Themed::default(),
        ))
        .id();
    commands.entity(parent).add_child(entity);
    Ok(entity)
}

/// The row labels of a drawn tree (no indentation or markers: the row draws those).
pub fn tree_view_items(tree: &UiTree) -> Vec<ListItem> {
    tree.rows()
        .iter()
        .map(|r| ListItem {
            label: tree.label(r.node).unwrap_or("").to_owned(),
            subtitle: String::new(),
        })
        .collect()
}

/// The chevron a tree row shows: closed, open or nothing for a leaf.
pub fn chevron_for(has_children: bool, expanded: bool) -> &'static str {
    match (has_children, expanded) {
        (false, _) => "",
        (true, false) => CHEVRON_CLOSED,
        (true, true) => CHEVRON_OPEN,
    }
}

/// True when `x` (UI px from the row's left edge) hits the chevron of a row at `depth`.
pub fn hits_chevron(x: f32, depth: usize, indent: f32) -> bool {
    let left = depth as f32 * indent;
    x >= left && x < left + indent.max(12.0)
}

/// Spawns a drawn data grid of `grid` under `parent`, `height` UI px tall (header included).
pub fn spawn_data_grid(commands: &mut Commands, parent: Entity, grid: DataGrid, height: f32) -> Entity {
    let outer = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Points(height)],
                ..Default::default()
            }),
            Classes::one("data-grid"),
            Themed::default(),
        ))
        .id();
    let header = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Points(ROW_HEIGHT)],
                ..Default::default()
            }),
            Classes::one("data-grid-header"),
            Themed::default(),
            ChildOf(outer),
        ))
        .id();
    let columns = grid.columns.clone();
    let body = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Auto],
                flex_grow: 1.0,
                overflow: [Overflow::Clip, Overflow::Clip],
                ..Default::default()
            }),
            ScrollView::vertical(),
            ListView {
                item_height: ROW_HEIGHT,
                ..Default::default()
            },
            ListItems::default(),
            ListSelection::new(SelectionMode::Single),
            CustomEntries,
            grid,
            GridBody { header: Some(header) },
            ChildOf(outer),
        ))
        .id();
    let mut x = 0.0;
    for (column, col) in columns.iter().enumerate() {
        commands.spawn((
            UiNode,
            NodeStyle::new(abs(
                x,
                0.0,
                [Dimension::Points(col.width), Dimension::Points(ROW_HEIGHT)],
            )),
            Interactable::default(),
            crate::focus::Focusable::default(),
            GridHeaderCell { body, column },
            Classes::one("data-grid-heading"),
            Themed::default(),
            text(&col.title),
            ChildOf(header),
        ));
        x += col.width;
    }
    commands.entity(outer).insert(DataGridView { body, header });
    commands.entity(parent).add_child(outer);
    outer
}

/// The header label of `column`: its title plus the sort arrow when the grid sorts by it.
pub fn header_label(grid: &DataGrid, column: usize) -> String {
    let title = grid.columns.get(column).map_or("", |c| c.title.as_str());
    match grid.sort {
        Some((c, true)) if c == column => format!("{title} {SORT_ASC}"),
        Some((c, false)) if c == column => format!("{title} {SORT_DESC}"),
        _ => title.to_owned(),
    }
}

/// Height of the color picker's hue and alpha bars, UI px: the 24 px minimum target (WCAG 2.2 SC 2.5.8).
const PICKER_BAR: f32 = 24.0;

/// Spawns a drawn color picker for `picker` under `parent`: a `square` UI px saturation/value
/// square, 24 px hue and alpha bars below it and a swatch with the hex label.
pub fn spawn_color_picker(
    commands: &mut Commands,
    parent: Entity,
    picker: ColorPicker,
    square: f32,
) -> Entity {
    let owner = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                gap: [Dimension::Points(6.0), Dimension::Points(6.0)],
                size: [Dimension::Points(square), Dimension::Auto],
                ..Default::default()
            }),
            picker,
            Classes::one("color-picker"),
            Themed::default(),
        ))
        .id();
    let part = |commands: &mut Commands, part: PickerPart, h: f32| {
        commands
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Points(square), Dimension::Points(h)],
                    ..Default::default()
                }),
                Interactable::default(),
                part,
                PickerOwner(owner),
                crate::a11y::Accessible::default().with_label(match part {
                    PickerPart::Square => "Saturation and value",
                    PickerPart::Hue => "Hue",
                    PickerPart::Alpha => "Alpha",
                }),
                NodeName(format!("color_picker.{part:?}").to_lowercase()),
                ChildOf(owner),
            ))
            .id()
    };
    let fill = || Style {
        position: PositionType::Absolute,
        inset: Edges::all(Dimension::Points(0.0)),
        size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
        ..Default::default()
    };
    let sq = part(commands, PickerPart::Square, square);
    let square_white = commands.spawn((UiNode, NodeStyle::new(fill()), ChildOf(sq))).id();
    commands.spawn((
        UiNode,
        NodeStyle::new(fill()),
        Background {
            kind: BackgroundKind::Gradient {
                start: UiColor::TRANSPARENT,
                end: UiColor([0.0, 0.0, 0.0, 1.0]),
                angle: 0.0,
            },
        },
        ChildOf(sq),
    ));
    let hue = part(commands, PickerPart::Hue, PICKER_BAR);
    // Six two-stop segments make the hue ramp (red, yellow, green, cyan, blue, magenta, red).
    let stops = [
        [1.0, 0.0, 0.0],
        [1.0, 1.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 1.0, 1.0],
        [0.0, 0.0, 1.0],
        [1.0, 0.0, 1.0],
        [1.0, 0.0, 0.0],
    ];
    let seg = square / 6.0;
    for (i, pair) in stops.windows(2).enumerate() {
        let c = |s: [f32; 3]| UiColor([s[0], s[1], s[2], 1.0]);
        commands.spawn((
            UiNode,
            NodeStyle::new(abs(
                i as f32 * seg,
                0.0,
                [Dimension::Points(seg + 0.5), Dimension::Percent(100.0)],
            )),
            Background {
                kind: BackgroundKind::Gradient {
                    start: c(pair[0]),
                    end: c(pair[1]),
                    angle: 90.0,
                },
            },
            ChildOf(hue),
        ));
    }
    let alpha = part(commands, PickerPart::Alpha, PICKER_BAR);
    let marker = |commands: &mut Commands, on: Entity, size: f32| {
        commands
            .spawn((
                UiNode,
                NodeStyle::new(abs(0.0, 0.0, [Dimension::Points(size), Dimension::Points(size)])),
                Border {
                    color: UiColor::WHITE,
                    widths: [2.0; 4],
                    radius: [size * 0.5; 4],
                },
                ChildOf(on),
            ))
            .id()
    };
    let square_marker = marker(commands, sq, 12.0);
    let hue_marker = marker(commands, hue, PICKER_BAR);
    let alpha_marker = marker(commands, alpha, PICKER_BAR);
    let row = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                align_items: Align::Center,
                gap: [Dimension::Points(8.0), Dimension::Points(8.0)],
                ..Default::default()
            }),
            ChildOf(owner),
        ))
        .id();
    let swatch = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(28.0), Dimension::Points(28.0)],
                ..Default::default()
            }),
            Border {
                color: UiColor([1.0, 1.0, 1.0, 0.4]),
                widths: [1.0; 4],
                radius: [4.0; 4],
            },
            ChildOf(row),
        ))
        .id();
    let label = commands
        .spawn((
            UiNode,
            NodeStyle::default(),
            text(&picker.to_hex()),
            NodeName("color_picker.hex".into()),
            ChildOf(row),
        ))
        .id();
    commands.entity(owner).insert(ColorPickerView {
        square: sq,
        square_white,
        hue,
        alpha,
        square_marker,
        hue_marker,
        alpha_marker,
        swatch,
        label,
    });
    commands.entity(parent).add_child(owner);
    owner
}

/// Spawns a [`Separator`] under `parent`.
pub fn spawn_separator(commands: &mut Commands, parent: Entity, vertical: bool) -> Entity {
    let size = if vertical {
        [Dimension::Points(1.0), Dimension::Percent(100.0)]
    } else {
        [Dimension::Percent(100.0), Dimension::Points(1.0)]
    };
    let e = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size,
                margin: Edges::all(Dimension::Points(4.0)),
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor([1.0, 1.0, 1.0, 0.16])),
            },
            Separator { vertical },
            Classes::one("separator"),
            Themed::default(),
        ))
        .id();
    commands.entity(parent).add_child(e);
    e
}

/// The fraction `[0, 1]` of `rect` (x, y, w, h) a point is at, clamped.
pub fn fraction_in(rect: [f32; 4], p: [f32; 2]) -> [f32; 2] {
    let f = |v: f32, o: f32, len: f32| {
        if len > 0.0 {
            ((v - o) / len).clamp(0.0, 1.0)
        } else {
            0.0
        }
    };
    [f(p[0], rect[0], rect[2]), f(p[1], rect[1], rect[3])]
}

/// Applies a pointer at `p` (physical px) on `part` of a picker whose part rect is `rect`.
pub fn pick_at(picker: &mut ColorPicker, part: PickerPart, rect: [f32; 4], p: [f32; 2]) {
    let [fx, fy] = fraction_in(rect, p);
    match part {
        PickerPart::Square => picker.pick_square(fx, fy),
        PickerPart::Hue => picker.pick_hue(fx),
        PickerPart::Alpha => picker.pick_alpha(fx),
    }
}

/// `Update`, after the widget systems: refreshes tree and grid list items when their model
/// changed, mirrors the tree selection into the list selection, and handles clicks on tree rows,
/// grid headers and color picker parts.
pub fn parity_widget_input(
    mut events: MessageReader<UiEvent>,
    pointer: Option<Res<UiPointer>>,
    rows: Query<(&ListRow, &ComputedNode)>,
    headers: Query<&GridHeaderCell>,
    parts: Query<(&PickerPart, &PickerOwner, &ComputedNode)>,
    mut trees: Query<(&mut UiTree, &TreeView)>,
    mut grids: Query<&mut DataGrid, With<GridBody>>,
    mut pickers: Query<&mut ColorPicker>,
) {
    let cursor = pointer.as_ref().and_then(|p| p.cursor);
    for event in events.read() {
        let toggle_click = matches!(event.kind, UiEventKind::DoubleClick);
        let press = matches!(
            event.kind,
            UiEventKind::Click | UiEventKind::DragStart | UiEventKind::Drag(_)
        );
        if (press || toggle_click)
            && let Ok((row, computed)) = rows.get(event.target)
            && let Ok((mut tree, view)) = trees.get_mut(row.list)
        {
            let Some(r) = tree.rows().get(row.index).copied() else {
                continue;
            };
            if matches!(event.kind, UiEventKind::Click) {
                let scale = computed.scale.max(0.001);
                let on_chevron = cursor
                    .is_some_and(|c| hits_chevron((c[0] - computed.rect[0]) / scale, r.depth, view.indent));
                if on_chevron && r.has_children {
                    tree.toggle(r.node);
                }
                tree.select(r.node);
            } else if toggle_click && r.has_children {
                tree.toggle(r.node);
            }
            continue;
        }
        if matches!(event.kind, UiEventKind::Click)
            && let Ok(cell) = headers.get(event.target)
            && let Ok(mut grid) = grids.get_mut(cell.body)
        {
            grid.sort_by(cell.column);
            continue;
        }
        if press
            && let Ok((part, owner, computed)) = parts.get(event.target)
            && let Some(c) = cursor
            && let Ok(mut picker) = pickers.get_mut(owner.0)
        {
            pick_at(&mut picker, *part, computed.rect, c);
        }
    }
}

/// `Update`: writes the visible rows of changed drawn trees and grids into their list items
/// (marking the list for rebinding), mirrors the tree selection into the list selection, and
/// keeps the grid header labels in step with the sort.
pub fn sync_parity_lists(
    mut trees: Query<
        (
            &mut UiTree,
            &mut ListItems,
            &mut ListView,
            Option<&mut ListSelection>,
        ),
        With<TreeView>,
    >,
    mut grids: Query<(Ref<DataGrid>, &GridBody, &mut ListItems, &mut ListView), Without<TreeView>>,
    cells: Query<(&GridHeaderCell, Entity)>,
    mut texts: Query<&mut Text>,
) {
    let _span = tracing::trace_span!("ui_parity_lists").entered();
    for (mut tree, mut items, mut list, selection) in &mut trees {
        if !tree.take_change() {
            continue;
        }
        items.0 = tree_view_items(&tree);
        list.dirty = true;
        if let Some(mut sel) = selection {
            let row = tree.selected.and_then(|n| tree.row_of(n));
            let want: Vec<usize> = row.into_iter().collect();
            if sel.to_vec() != want {
                sel.set(&want);
            }
        }
    }
    for (grid, _, mut items, mut list) in &mut grids {
        if !grid.is_changed() {
            continue;
        }
        items.0 = grid
            .order()
            .into_iter()
            .map(|r| ListItem {
                label: grid.rows[r].join(" "),
                subtitle: String::new(),
            })
            .collect();
        list.dirty = true;
    }
    // Header labels follow the sort of their own grid.
    for (cell, entity) in &cells {
        let Ok((grid, _, _, _)) = grids.get(cell.body) else {
            continue;
        };
        let label = header_label(&grid, cell.column);
        if let Ok(mut t) = texts.get_mut(entity)
            && !shows(&t, &label)
        {
            relabel(&mut t, &label);
        }
    }
}

/// `Update`, after [`sync_parity_lists`]: fills rebound tree and grid rows with their chevron,
/// label and cells.
pub fn fill_parity_rows(
    mut commands: Commands,
    mut binds: MessageReader<ListEntryBind>,
    trees: Query<(&UiTree, &TreeView)>,
    grids: Query<&DataGrid, With<GridBody>>,
    tree_parts: Query<&TreeRowParts>,
    grid_parts: Query<&GridRowParts>,
    mut nodes: Query<(&mut NodeStyle, &mut Text)>,
) {
    for bind in binds.read() {
        if let Ok((tree, view)) = trees.get(bind.list) {
            let Some(r) = tree.rows().get(bind.index).copied() else {
                continue;
            };
            let left = r.depth as f32 * view.indent;
            let chevron = chevron_for(r.has_children, r.expanded);
            let label = tree.label(r.node).unwrap_or("");
            let chevron_style = abs(
                left,
                4.0,
                [Dimension::Points(view.indent.max(12.0)), Dimension::Auto],
            );
            if view.reorder {
                // The payload follows the node the recycled row shows now.
                commands.entity(bind.row).insert(crate::dragdrop::DragSource::new(
                    crate::dragdrop::DragPayload::new(
                        TREE_DRAG_KIND,
                        crate::interaction::UiValue::Index(r.node),
                    ),
                ));
            }
            let label_style = abs(
                left + view.indent.max(12.0) + 4.0,
                4.0,
                [Dimension::Auto, Dimension::Auto],
            );
            match tree_parts.get(bind.row) {
                Ok(parts) => {
                    set_node(&mut nodes, parts.chevron, chevron_style, chevron);
                    set_node(&mut nodes, parts.label, label_style, label);
                }
                Err(_) => {
                    if view.reorder {
                        commands
                            .entity(bind.row)
                            .insert(crate::dragdrop::DropTarget::accepting([TREE_DRAG_KIND]));
                    }
                    let c = commands
                        .spawn((
                            UiNode,
                            NodeStyle::new(chevron_style),
                            text(chevron),
                            // The row's name is its label; a screen reader need not say the glyph.
                            crate::a11y::Accessible::decorative(),
                            ChildOf(bind.row),
                        ))
                        .id();
                    let l = commands
                        .spawn((
                            UiNode,
                            NodeStyle::new(label_style),
                            text(label),
                            ChildOf(bind.row),
                        ))
                        .id();
                    commands
                        .entity(bind.row)
                        .insert(TreeRowParts { chevron: c, label: l });
                }
            }
            continue;
        }
        if let Ok(grid) = grids.get(bind.list) {
            let order = grid.order();
            let Some(row) = order.get(bind.index).and_then(|r| grid.rows.get(*r)) else {
                continue;
            };
            let mut x = 0.0;
            let mut styles = Vec::with_capacity(grid.columns.len());
            for col in &grid.columns {
                styles.push(Style {
                    overflow: [Overflow::Clip, Overflow::Clip],
                    ..abs(
                        x + 6.0,
                        4.0,
                        [Dimension::Points((col.width - 8.0).max(1.0)), Dimension::Auto],
                    )
                });
                x += col.width;
            }
            let cell = |i: usize| row.get(i).map_or("", String::as_str);
            match grid_parts.get(bind.row) {
                Ok(parts) if parts.0.len() == styles.len() => {
                    for (i, (e, style)) in parts.0.iter().zip(styles).enumerate() {
                        set_node(&mut nodes, *e, style, cell(i));
                    }
                }
                other => {
                    if let Ok(old) = other {
                        for e in &old.0 {
                            commands.entity(*e).despawn();
                        }
                    }
                    let cells: Vec<Entity> = styles
                        .into_iter()
                        .enumerate()
                        .map(|(i, style)| {
                            commands
                                .spawn((UiNode, NodeStyle::new(style), text(cell(i)), ChildOf(bind.row)))
                                .id()
                        })
                        .collect();
                    commands.entity(bind.row).insert(GridRowParts(cells));
                }
            }
        }
    }
}

fn set_node(nodes: &mut Query<(&mut NodeStyle, &mut Text)>, e: Entity, style: Style, label: &str) {
    if let Ok((mut s, mut t)) = nodes.get_mut(e) {
        if s.0 != style {
            s.0 = style;
        }
        if !shows(&t, label) {
            relabel(&mut t, label);
        }
    }
}

/// `Update`: redraws changed color pickers: the square's hue, the alpha ramp, the markers, the
/// swatch and the hex label.
pub fn sync_color_pickers(
    pickers: Query<(&ColorPicker, &ColorPickerView), Changed<ColorPicker>>,
    computed: Query<&ComputedNode>,
    mut backgrounds: Query<&mut Background>,
    mut styles: Query<&mut NodeStyle>,
    mut texts: Query<&mut Text>,
    mut commands: Commands,
) {
    for (picker, view) in &pickers {
        let pure = ColorPicker {
            sat: 1.0,
            val: 1.0,
            alpha: 1.0,
            ..*picker
        }
        .color();
        let opaque = ColorPicker {
            alpha: 1.0,
            ..*picker
        }
        .color();
        let mut set_bg = |e: Entity, kind: BackgroundKind| match backgrounds.get_mut(e) {
            Ok(mut bg) => {
                if bg.kind != kind {
                    bg.kind = kind;
                }
            }
            Err(_) => {
                commands.entity(e).insert(Background { kind });
            }
        };
        set_bg(view.square, BackgroundKind::Color(pure));
        set_bg(
            view.square_white,
            BackgroundKind::Gradient {
                start: UiColor::WHITE,
                end: UiColor([1.0, 1.0, 1.0, 0.0]),
                angle: 90.0,
            },
        );
        set_bg(
            view.alpha,
            BackgroundKind::Gradient {
                start: UiColor([opaque.0[0], opaque.0[1], opaque.0[2], 0.0]),
                end: opaque,
                angle: 90.0,
            },
        );
        set_bg(view.swatch, BackgroundKind::Color(picker.color()));
        let size_of = |e: Entity| {
            computed.get(e).map_or([0.0, 0.0], |c| {
                let s = c.scale.max(0.001);
                [c.rect[2] / s, c.rect[3] / s]
            })
        };
        let place =
            |styles: &mut Query<&mut NodeStyle>, marker: Entity, part: Entity, fx: f32, fy: f32, m: f32| {
                let [w, h] = size_of(part);
                if let Ok(mut s) = styles.get_mut(marker) {
                    let left = Dimension::Points(fx * w - m * 0.5);
                    let top = Dimension::Points(fy * h - m * 0.5);
                    if s.0.inset.left != left || s.0.inset.top != top {
                        s.0.inset.left = left;
                        s.0.inset.top = top;
                    }
                }
            };
        let (sx, sy) = picker.square_pos();
        place(&mut styles, view.square_marker, view.square, sx, sy, 12.0);
        place(
            &mut styles,
            view.hue_marker,
            view.hue,
            picker.hue_pos(),
            0.5,
            PICKER_BAR,
        );
        place(
            &mut styles,
            view.alpha_marker,
            view.alpha,
            picker.alpha,
            0.5,
            PICKER_BAR,
        );
        if let Ok(mut t) = texts.get_mut(view.label) {
            let hex = picker.to_hex();
            if !shows(&t, &hex) {
                relabel(&mut t, &hex);
            }
        }
    }
}

/// The drop position of a pointer at `y` (physical px) over a row whose rect is `rect`.
pub fn row_drop_position(rect: [f32; 4], y: f32) -> crate::tree::DropPosition {
    let f = if rect[3] > 0.0 {
        (y - rect[1]) / rect[3]
    } else {
        0.5
    };
    UiTree::drop_position(f)
}

/// `Update`, after drag and drop: applies drops of tree rows on tree rows (sending
/// [`TreeDropped`]) and keeps the drop indicator on the hovered row.
pub fn tree_view_drops(
    mut commands: Commands,
    mut drops: MessageReader<crate::dragdrop::DragDropEvent>,
    pointer: Option<Res<UiPointer>>,
    rows: Query<(&ListRow, &ComputedNode, &NodeStyle)>,
    hovered: Query<Entity, With<crate::dragdrop::DropHover>>,
    mut trees: Query<(&mut UiTree, &TreeView, Option<&TreeDropIndicator>)>,
    mut styles: Query<&mut NodeStyle, Without<ListRow>>,
    mut out: MessageWriter<TreeDropped>,
) {
    let cursor = pointer.as_ref().and_then(|p| p.cursor);
    for event in drops.read() {
        let crate::dragdrop::DragDropEvent::Dropped { target, payload, .. } = event else {
            continue;
        };
        let crate::interaction::UiValue::Index(source) = payload.data else {
            continue;
        };
        if payload.kind != TREE_DRAG_KIND {
            continue;
        }
        let Ok((row, computed, _)) = rows.get(*target) else {
            continue;
        };
        let Ok((mut tree, view, _)) = trees.get_mut(row.list) else {
            continue;
        };
        let Some(target_node) = tree.rows().get(row.index).map(|r| r.node) else {
            continue;
        };
        let position = cursor.map_or(crate::tree::DropPosition::Inside, |c| {
            row_drop_position(computed.rect, c[1])
        });
        let moved = view.reorder && tree.move_node(source, target_node, position);
        out.write(TreeDropped {
            tree: row.list,
            source,
            target: target_node,
            position,
            moved,
        });
    }
    // Drop indicator: one per tree, on the hovered row of that tree.
    let hover = hovered.iter().find_map(|e| rows.get(e).ok().map(|r| (e, r)));
    let mut placed: Option<Entity> = None;
    if let Some((_, (row, computed, row_style))) = hover
        && let Ok((_, view, indicator)) = trees.get(row.list)
        && view.reorder
    {
        let top = match row_style.0.inset.top {
            Dimension::Points(t) => t,
            _ => 0.0,
        };
        let h = match row_style.0.size[1] {
            Dimension::Points(h) => h,
            _ => ROW_HEIGHT,
        };
        let position = cursor.map_or(crate::tree::DropPosition::Inside, |c| {
            row_drop_position(computed.rect, c[1])
        });
        let style = match position {
            crate::tree::DropPosition::Before => abs(
                0.0,
                top - 1.0,
                [Dimension::Percent(100.0), Dimension::Points(2.0)],
            ),
            crate::tree::DropPosition::After => abs(
                0.0,
                top + h - 1.0,
                [Dimension::Percent(100.0), Dimension::Points(2.0)],
            ),
            crate::tree::DropPosition::Inside => {
                abs(0.0, top, [Dimension::Percent(100.0), Dimension::Points(h)])
            }
        };
        match indicator {
            Some(ind) => {
                if let Ok(mut s) = styles.get_mut(ind.0)
                    && s.0 != style
                {
                    s.0 = style;
                }
                placed = Some(ind.0);
            }
            None => {
                let bar = commands
                    .spawn((
                        UiNode,
                        NodeStyle::new(style),
                        Background {
                            kind: BackgroundKind::Color(UiColor([0.35, 0.65, 1.0, 0.45])),
                        },
                        crate::node::PickThrough,
                        ChildOf(row.list),
                    ))
                    .id();
                commands.entity(row.list).insert(TreeDropIndicator(bar));
                placed = Some(bar);
            }
        }
    }
    // Hide every other indicator.
    for (_, _, indicator) in &trees {
        if let Some(ind) = indicator
            && Some(ind.0) != placed
            && let Ok(mut s) = styles.get_mut(ind.0)
            && s.0.display != crate::style::Display::None
        {
            s.0.display = crate::style::Display::None;
        }
    }
}

/// A search box (ADR-273 W11, ADR-2045): a [`crate::text_input::TextInput`] on the same
/// entity whose text filters `items` into the [`ListItems`] of `target` with the fuzzy ranker
/// (best first; an empty query lists every item in order). [`SearchBox::shown`] maps each shown
/// row back to its item index.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct SearchBox {
    /// The list entity the results go to.
    pub target: Option<Entity>,
    /// The candidate labels.
    pub items: Vec<String>,
    /// Most results shown (0 = all).
    pub limit: usize,
    /// Item index of each shown row (written by [`sync_search_boxes`]).
    pub shown: Vec<usize>,
    /// The query the results were computed for.
    pub query: String,
}

/// The item indexes a search box shows for `query`: every item in order for an empty query,
/// else the fuzzy ranking (`crate::fuzzy::fuzzy_rank`).
pub fn search_results(items: &[String], query: &str, limit: usize) -> Vec<usize> {
    if query.trim().is_empty() {
        let n = if limit == 0 {
            items.len()
        } else {
            limit.min(items.len())
        };
        return (0..n).collect();
    }
    crate::fuzzy::fuzzy_rank(query.trim(), items, limit)
        .into_iter()
        .map(|m| m.index)
        .collect()
}

// How many times `sync_search_boxes` actually ranked. A caret blink must not move it.
#[cfg(test)]
thread_local! {
    static SEARCH_RANKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_search_ranks() {
    SEARCH_RANKS.with(|c| c.set(0));
}

#[cfg(test)]
fn search_ranks() -> u64 {
    SEARCH_RANKS.with(|c| c.get())
}

#[inline]
fn note_search_rank() {
    #[cfg(test)]
    SEARCH_RANKS.with(|c| c.set(c.get().saturating_add(1)));
}

/// `Update`: refilters every search box whose query, item list, or limit changed.
///
/// A focused field's caret blink writes `TextInput` every frame. That change is not a new query,
/// so it does not rank again. Ranking a long list can exceed a tenth of a millisecond; the span
/// is `ui_sync_search_boxes`, and the rank itself spans `ui_fuzzy_rank`.
pub fn sync_search_boxes(
    mut boxes: Query<(Ref<crate::text_input::TextInput>, &mut SearchBox)>,
    mut lists: Query<(&mut ListItems, Option<&mut ListView>)>,
) {
    let _span = tracing::trace_span!("ui_sync_search_boxes").entered();
    for (input, mut sb) in &mut boxes {
        let query_changed = input.is_changed() && sb.query != input.text;
        if !query_changed && !sb.is_changed() {
            continue;
        }
        note_search_rank();
        let shown = search_results(&sb.items, &input.text, sb.limit);
        let target = sb.target;
        if sb.shown != shown || sb.query != input.text {
            sb.query.clone_from(&input.text);
            sb.shown = shown;
        }
        if let Some(t) = target
            && let Ok((mut items, list)) = lists.get_mut(t)
        {
            let next: Vec<ListItem> = sb
                .shown
                .iter()
                .filter_map(|i| sb.items.get(*i))
                .map(|l| ListItem {
                    label: l.clone(),
                    subtitle: String::new(),
                })
                .collect();
            if items.0 != next {
                items.0 = next;
                if let Some(mut list) = list {
                    list.dirty = true;
                }
            }
        }
    }
}

/// A progress dialog (ADR-273 W11, ADR-2045): a [`crate::widgets::Modal`] with a title, a
/// [`crate::widgets::ProgressBar`], a status line and an optional cancel button. Set
/// [`Self::progress`] and [`Self::status`]; [`sync_progress_dialogs`] updates the parts. A
/// click on the cancel button sets [`Self::cancelled`] and closes the modal.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct ProgressDialog {
    /// Progress `0..=1`.
    pub progress: f32,
    /// Status line under the bar.
    pub status: String,
    /// The player pressed cancel.
    pub cancelled: bool,
    /// The bar entity.
    pub bar: Option<Entity>,
    /// The status text entity.
    pub status_text: Option<Entity>,
    /// The cancel button, if any.
    pub cancel: Option<Entity>,
}

/// Marks the cancel button of a [`ProgressDialog`].
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressCancel(pub Entity);

/// Spawns a progress dialog under `parent` (usually a UI root): modal, centred panel, `title`,
/// a bar, a status line and, when `cancellable`, a Cancel button.
pub fn spawn_progress_dialog(
    commands: &mut Commands,
    parent: Entity,
    title: &str,
    cancellable: bool,
) -> Entity {
    let dialog = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                gap: [Dimension::Points(10.0), Dimension::Points(10.0)],
                padding: Edges::all(Dimension::Points(16.0)),
                size: [Dimension::Points(360.0), Dimension::Auto],
                ..Default::default()
            }),
            crate::widgets::Modal::default(),
            Classes(vec!["panel".into(), "progress-dialog".into()]),
            Themed::default(),
        ))
        .id();
    commands.spawn((UiNode, text(title), ChildOf(dialog)));
    let bar = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Points(10.0)],
                ..Default::default()
            }),
            crate::widgets::ProgressBar::default(),
            Classes::one("progress"),
            Themed::default(),
            ChildOf(dialog),
        ))
        .id();
    let status_text = commands.spawn((UiNode, text(""), ChildOf(dialog))).id();
    let cancel = cancellable.then(|| {
        let b = crate::widgets::spawn_button(commands, dialog, "Cancel", &[]);
        commands.entity(b).insert(ProgressCancel(dialog));
        b
    });
    commands.entity(dialog).insert(ProgressDialog {
        bar: Some(bar),
        status_text: Some(status_text),
        cancel,
        ..Default::default()
    });
    commands.entity(parent).add_child(dialog);
    dialog
}

/// `Update`: mirrors changed progress dialogs into their bar and status line, and handles the
/// cancel button.
pub fn sync_progress_dialogs(
    mut events: MessageReader<UiEvent>,
    cancels: Query<&ProgressCancel>,
    mut dialogs: Query<(&mut ProgressDialog, Option<&mut crate::widgets::Modal>)>,
    mut bars: Query<&mut crate::widgets::ProgressBar>,
    mut texts: Query<&mut Text>,
) {
    for event in events.read() {
        if matches!(event.kind, UiEventKind::Click)
            && let Ok(c) = cancels.get(event.target)
            && let Ok((mut d, modal)) = dialogs.get_mut(c.0)
        {
            d.cancelled = true;
            if let Some(mut m) = modal {
                m.open = false;
            }
        }
    }
    for (d, _) in &mut dialogs {
        if !d.is_changed() {
            continue;
        }
        let value = if d.progress.is_finite() {
            d.progress.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if let Some(mut bar) = d.bar.and_then(|b| bars.get_mut(b).ok())
            && bar.value != value
        {
            bar.value = value;
        }
        if let Some(mut t) = d.status_text.and_then(|e| texts.get_mut(e).ok())
            && !shows(&t, &d.status)
        {
            relabel(&mut t, &d.status);
        }
    }
}

/// Installs the drawn-widget systems. [`crate::UiPlugin`] adds it.
pub struct ParityWidgetsPlugin;

impl Plugin for ParityWidgetsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                parity_widget_input,
                crate::tree::sync_tree_lists,
                sync_search_boxes,
                sync_parity_lists,
                fill_parity_rows,
                sync_color_pickers,
                sync_progress_dialogs,
                tree_view_drops,
            )
                .chain()
                .after(crate::widgets::ui_widgets)
                .after(crate::dragdrop::ui_drag_drop),
        )
        .add_message::<TreeDropped>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::GridColumn;

    fn app() -> App {
        let mut app = App::new();
        app.add_message::<UiEvent>()
            .add_message::<ListEntryBind>()
            .init_resource::<UiPointer>()
            .add_systems(
                Update,
                (
                    parity_widget_input,
                    crate::tree::sync_tree_lists,
                    sync_parity_lists,
                    fill_parity_rows,
                    sync_color_pickers,
                    sync_progress_dialogs,
                )
                    .chain(),
            );
        app
    }

    fn node(rect: [f32; 4]) -> ComputedNode {
        ComputedNode {
            rect,
            scale: 1.0,
            ..Default::default()
        }
    }

    fn leaf(label: &str) -> TreeSpec {
        TreeSpec {
            label: label.into(),
            children: vec![],
        }
    }

    fn sample_tree() -> Vec<TreeSpec> {
        vec![
            TreeSpec {
                label: "Assets".into(),
                children: vec![leaf("rock.glb"), leaf("tree.glb")],
            },
            leaf("README"),
        ]
    }

    fn spawn_tree(app: &mut App) -> Entity {
        let world = app.world_mut();
        let parent = world.spawn(UiNode).id();
        let mut commands = world.commands();
        let e = spawn_tree_view(&mut commands, parent, &sample_tree(), 200.0).unwrap();
        world.flush();
        e
    }

    fn click(app: &mut App, target: Entity, at: [f32; 2], kind: UiEventKind) {
        app.world_mut().resource_mut::<UiPointer>().cursor = Some(at);
        app.world_mut().write_message(UiEvent::new(target, kind));
        app.update();
    }

    fn items(app: &App, e: Entity) -> Vec<String> {
        app.world()
            .get::<ListItems>(e)
            .unwrap()
            .0
            .iter()
            .map(|i| i.label.clone())
            .collect()
    }

    #[test]
    fn tree_rows_toggle_on_the_chevron_and_select_elsewhere() {
        let mut app = app();
        let list = spawn_tree(&mut app);
        app.update();
        assert_eq!(items(&app, list), ["Assets", "README"]);
        // Row 0 ("Assets", depth 0): the chevron spans x 0..16 of the row.
        let row = app
            .world_mut()
            .spawn((ListRow { list, index: 0 }, node([100.0, 50.0, 300.0, 28.0])))
            .id();
        click(&mut app, row, [105.0, 60.0], UiEventKind::Click);
        assert_eq!(items(&app, list), ["Assets", "rock.glb", "tree.glb", "README"]);
        let tree = app.world().get::<UiTree>(list).unwrap();
        assert_eq!(tree.selected, Some(0));
        assert_eq!(app.world().get::<ListSelection>(list).unwrap().to_vec(), [0]);
        // A click on the label only selects; a double click toggles.
        click(&mut app, row, [180.0, 60.0], UiEventKind::Click);
        assert_eq!(items(&app, list).len(), 4);
        click(&mut app, row, [180.0, 60.0], UiEventKind::DoubleClick);
        assert_eq!(items(&app, list), ["Assets", "README"]);
    }

    #[test]
    fn tree_rows_fill_chevron_and_indented_label() {
        let mut app = app();
        let list = spawn_tree(&mut app);
        app.world_mut().get_mut::<UiTree>(list).unwrap().expand(0);
        app.update();
        let row = app.world_mut().spawn(UiNode).id();
        app.world_mut()
            .write_message(ListEntryBind { list, row, index: 0 });
        app.update();
        let parts = *app.world().get::<TreeRowParts>(row).unwrap();
        let label_of = |app: &App, e: Entity| match &app.world().get::<Text>(e).unwrap().spans[0].content {
            crate::text::TextContent::Lit(s) => s.clone(),
            other => format!("{other:?}"),
        };
        assert_eq!(label_of(&app, parts.chevron), CHEVRON_OPEN);
        assert_eq!(label_of(&app, parts.label), "Assets");
        // Rebinding the same row to a depth-1 leaf reuses the parts and indents them.
        app.world_mut()
            .write_message(ListEntryBind { list, row, index: 1 });
        app.update();
        assert_eq!(*app.world().get::<TreeRowParts>(row).unwrap(), parts);
        assert_eq!(label_of(&app, parts.chevron), "");
        assert_eq!(label_of(&app, parts.label), "rock.glb");
        let style = &app.world().get::<NodeStyle>(parts.chevron).unwrap().0;
        assert_eq!(style.inset.left, Dimension::Points(16.0));
    }

    #[test]
    fn chevron_hit_zone_follows_depth() {
        assert!(hits_chevron(3.0, 0, 16.0));
        assert!(!hits_chevron(17.0, 0, 16.0));
        assert!(hits_chevron(33.0, 2, 16.0));
        assert!(!hits_chevron(10.0, 2, 16.0));
        assert_eq!(chevron_for(false, true), "");
        assert_eq!(chevron_for(true, false), CHEVRON_CLOSED);
    }

    fn grid() -> DataGrid {
        DataGrid {
            columns: vec![
                GridColumn {
                    title: "Name".into(),
                    width: 120.0,
                },
                GridColumn {
                    title: "HP".into(),
                    width: 60.0,
                },
            ],
            rows: vec![
                vec!["orc".into(), "30".into()],
                vec!["elf".into(), "9".into()],
                vec!["bat".into(), "12".into()],
            ],
            sort: None,
        }
    }

    #[test]
    fn grid_header_click_sorts_and_rows_fill_cells() {
        let mut app = app();
        let world = app.world_mut();
        let parent = world.spawn(UiNode).id();
        let mut commands = world.commands();
        let outer = spawn_data_grid(&mut commands, parent, grid(), 300.0);
        world.flush();
        app.update();
        let view = *app.world().get::<DataGridView>(outer).unwrap();
        assert_eq!(items(&app, view.body), ["orc 30", "elf 9", "bat 12"]);
        let hp = app
            .world_mut()
            .query::<(Entity, &GridHeaderCell)>()
            .iter(app.world())
            .find(|(_, c)| c.column == 1)
            .map(|(e, _)| e)
            .unwrap();
        click(&mut app, hp, [0.0, 0.0], UiEventKind::Click);
        assert_eq!(items(&app, view.body), ["elf 9", "bat 12", "orc 30"]);
        let header = &app.world().get::<Text>(hp).unwrap().spans[0].content;
        assert_eq!(*header, crate::text::TextContent::Lit(format!("HP {SORT_ASC}")));
        click(&mut app, hp, [0.0, 0.0], UiEventKind::Click);
        assert_eq!(items(&app, view.body)[0], "orc 30");
        // A bound row gets one cell per column, in display order.
        let row = app.world_mut().spawn(UiNode).id();
        app.world_mut().write_message(ListEntryBind {
            list: view.body,
            row,
            index: 2,
        });
        app.update();
        let cells = app.world().get::<GridRowParts>(row).unwrap().0.clone();
        assert_eq!(cells.len(), 2);
        let first = &app.world().get::<Text>(cells[0]).unwrap().spans[0].content;
        assert_eq!(*first, crate::text::TextContent::Lit("elf".into()));
        assert_eq!(
            app.world().get::<NodeStyle>(cells[1]).unwrap().0.inset.left,
            Dimension::Points(126.0)
        );
    }

    #[test]
    fn color_picker_parts_pick_and_redraw() {
        let mut app = app();
        let world = app.world_mut();
        let parent = world.spawn(UiNode).id();
        let mut commands = world.commands();
        let owner = spawn_color_picker(&mut commands, parent, ColorPicker::default(), 160.0);
        world.flush();
        let view = *app.world().get::<ColorPickerView>(owner).unwrap();
        app.world_mut()
            .entity_mut(view.square)
            .insert(node([10.0, 10.0, 160.0, 160.0]));
        app.world_mut()
            .entity_mut(view.hue)
            .insert(node([10.0, 176.0, 160.0, 16.0]));
        app.world_mut()
            .entity_mut(view.alpha)
            .insert(node([10.0, 198.0, 160.0, 16.0]));
        // Hue bar at 1/3 → green; square top-right → full saturation and value.
        click(
            &mut app,
            view.hue,
            [10.0 + 160.0 / 3.0, 180.0],
            UiEventKind::Click,
        );
        click(&mut app, view.square, [170.0, 10.0], UiEventKind::DragStart);
        let picker = *app.world().get::<ColorPicker>(owner).unwrap();
        assert_eq!(picker.to_hex(), "#00FF00");
        let label = &app.world().get::<Text>(view.label).unwrap().spans[0].content;
        assert_eq!(*label, crate::text::TextContent::Lit("#00FF00".into()));
        // Alpha at the left end → transparent; the swatch follows.
        click(&mut app, view.alpha, [0.0, 200.0], UiEventKind::Drag([1.0, 0.0]));
        let picker = *app.world().get::<ColorPicker>(owner).unwrap();
        assert_eq!(picker.alpha, 0.0);
        let swatch = app.world().get::<Background>(view.swatch).unwrap();
        assert_eq!(swatch.kind, BackgroundKind::Color(picker.color()));
        let marker = &app.world().get::<NodeStyle>(view.square_marker).unwrap().0;
        assert_eq!(marker.inset.left, Dimension::Points(160.0 - 6.0));
    }

    #[test]
    fn fraction_in_clamps() {
        assert_eq!(fraction_in([10.0, 10.0, 100.0, 50.0], [60.0, 35.0]), [0.5, 0.5]);
        assert_eq!(fraction_in([10.0, 10.0, 100.0, 50.0], [-5.0, 99.0]), [0.0, 1.0]);
        assert_eq!(fraction_in([0.0, 0.0, 0.0, 0.0], [5.0, 5.0]), [0.0, 0.0]);
    }

    #[test]
    fn search_box_filters_its_list() {
        let mut app = app();
        app.add_systems(Update, sync_search_boxes);
        let list = app
            .world_mut()
            .spawn((ListItems::default(), ListView::default()))
            .id();
        let labels: Vec<String> = ["Open file", "Save file", "Close window", "Settings"]
            .into_iter()
            .map(String::from)
            .collect();
        let sb = app
            .world_mut()
            .spawn((
                crate::text_input::TextInput::default(),
                SearchBox {
                    target: Some(list),
                    items: labels,
                    ..Default::default()
                },
            ))
            .id();
        app.update();
        assert_eq!(items(&app, list).len(), 4, "an empty query lists everything");
        app.world_mut()
            .get_mut::<crate::text_input::TextInput>(sb)
            .unwrap()
            .text = "file".into();
        app.update();
        assert_eq!(items(&app, list), ["Open file", "Save file"]);
        assert_eq!(app.world().get::<SearchBox>(sb).unwrap().shown, [0, 1]);
        app.world_mut()
            .get_mut::<crate::text_input::TextInput>(sb)
            .unwrap()
            .text = "zzz".into();
        app.update();
        assert!(items(&app, list).is_empty());
        assert_eq!(search_results(&["a".into(), "b".into()], "", 1), [0]);
    }

    #[test]
    fn search_box_does_not_rerank_on_caret_blink() {
        use std::time::Duration;

        let mut app = app();
        app.insert_resource(crate::focus::FocusState::default())
            .insert_resource(crate::layout::LayoutDirty::default())
            .insert_resource(Time::default());
        app.world_mut()
            .resource_mut::<Time>()
            .advance(Duration::from_millis(100));
        assert!(app.world().resource::<Time>().delta_secs() > 0.0);
        app.add_systems(
            Update,
            (crate::text_input::ui_text_inputs, sync_search_boxes).chain(),
        );
        // The rank counter is thread-local. On this host the default executor runs systems on
        // the compute pool, so pin them to the calling thread or the counter stays at zero.
        app.set_schedule_executor(regia_app::ScheduleExecutor::SingleThreaded);
        let list = app
            .world_mut()
            .spawn((ListItems::default(), ListView::default()))
            .id();
        let labels: Vec<String> = (0..400)
            .map(|i: usize| {
                if i.is_multiple_of(2) {
                    format!("switch_{i:03}")
                } else {
                    format!("other_{i:03}")
                }
            })
            .collect();
        let mut field = crate::text_input::TextInput::default();
        field.set_text("sw");
        let sb = app
            .world_mut()
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Points(240.0), Dimension::Points(36.0)],
                    ..Default::default()
                }),
                ComputedNode {
                    rect: [0.0, 0.0, 240.0, 36.0],
                    scale: 1.0,
                    ..Default::default()
                },
                field,
                SearchBox {
                    target: Some(list),
                    items: labels,
                    limit: 20,
                    ..Default::default()
                },
            ))
            .id();
        app.world_mut().resource_mut::<crate::focus::FocusState>().focused = Some(sb);
        reset_search_ranks();
        app.update();
        let expected: Vec<usize> = (0..40).step_by(2).collect();
        assert_eq!(search_ranks(), 1, "the query is ranked once when it is applied");
        assert_eq!(app.world().get::<SearchBox>(sb).unwrap().shown, expected);
        let rows = items(&app, list);
        assert_eq!(rows.len(), 20);
        // Second frame: the field stays focused and the caret blink advances. The query does not.
        app.update();
        assert_eq!(search_ranks(), 1, "a focused caret blink re-ranked {rows:?}");
        assert_eq!(app.world().get::<SearchBox>(sb).unwrap().shown, expected);
        assert_eq!(items(&app, list), rows);
        {
            let mut box_mut = app.world_mut().get_mut::<SearchBox>(sb).unwrap();
            box_mut.items[0] = "unrelated".into();
        }
        app.update();
        assert_eq!(search_ranks(), 2, "editing the candidate list must rank again");
    }

    #[test]
    fn progress_dialog_mirrors_progress_and_cancels() {
        let mut app = app();
        let world = app.world_mut();
        let parent = world.spawn(UiNode).id();
        let mut commands = world.commands();
        let d = spawn_progress_dialog(&mut commands, parent, "Baking", true);
        world.flush();
        {
            let mut dialog = app.world_mut().get_mut::<ProgressDialog>(d).unwrap();
            dialog.progress = 0.4;
            dialog.status = "cell 4 of 10".into();
        }
        app.update();
        let dialog = app.world().get::<ProgressDialog>(d).unwrap().clone();
        let bar = app
            .world()
            .get::<crate::widgets::ProgressBar>(dialog.bar.unwrap())
            .unwrap();
        assert!((bar.value - 0.4).abs() < 1e-6);
        let status = &app
            .world()
            .get::<Text>(dialog.status_text.unwrap())
            .unwrap()
            .spans[0]
            .content;
        assert_eq!(*status, crate::text::TextContent::Lit("cell 4 of 10".into()));
        click(&mut app, dialog.cancel.unwrap(), [0.0, 0.0], UiEventKind::Click);
        assert!(app.world().get::<ProgressDialog>(d).unwrap().cancelled);
        assert!(!app.world().get::<crate::widgets::Modal>(d).unwrap().open);
    }

    #[test]
    fn tree_rows_drop_onto_rows_and_show_an_indicator() {
        use crate::dragdrop::{DragDropEvent, DragPayload, DropHover};
        use crate::interaction::UiValue;
        let mut app = app();
        app.add_message::<DragDropEvent>()
            .add_message::<TreeDropped>()
            .add_systems(Update, tree_view_drops);
        let list = spawn_tree(&mut app);
        {
            let mut view = app.world_mut().get_mut::<TreeView>(list).unwrap();
            view.reorder = true;
        }
        app.world_mut().get_mut::<UiTree>(list).unwrap().expand_all();
        app.update();
        assert_eq!(items(&app, list), ["Assets", "rock.glb", "tree.glb", "README"]);
        // Drop README (node 3) onto the top quarter of the "rock.glb" row (row 1): before it.
        let row_style = NodeStyle::new(abs(
            0.0,
            28.0,
            [Dimension::Percent(100.0), Dimension::Points(28.0)],
        ));
        let target = app
            .world_mut()
            .spawn((
                ListRow { list, index: 1 },
                node([0.0, 28.0, 300.0, 28.0]),
                row_style,
            ))
            .id();
        let source = app.world_mut().spawn(UiNode).id();
        app.world_mut().resource_mut::<UiPointer>().cursor = Some([50.0, 30.0]);
        app.world_mut().write_message(DragDropEvent::Dropped {
            source,
            target,
            payload: DragPayload::new(TREE_DRAG_KIND, UiValue::Index(3)),
        });
        app.update();
        assert_eq!(items(&app, list), ["Assets", "README", "rock.glb", "tree.glb"]);
        let sent: Vec<TreeDropped> = app
            .world_mut()
            .resource_mut::<Messages<TreeDropped>>()
            .drain()
            .collect();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].moved);
        assert_eq!((sent[0].source, sent[0].target), (3, 1));
        // Hovering a row during a drag places the indicator: a bar at the row's bottom edge.
        app.world_mut()
            .entity_mut(target)
            .insert(DropHover { accepted: true });
        app.world_mut().resource_mut::<UiPointer>().cursor = Some([50.0, 54.0]);
        app.update();
        let bar = app.world().get::<TreeDropIndicator>(list).unwrap().0;
        let style = &app.world().get::<NodeStyle>(bar).unwrap().0;
        assert_eq!(style.inset.top, Dimension::Points(55.0));
        // No hover: hidden.
        app.world_mut().entity_mut(target).remove::<DropHover>();
        app.update();
        assert_eq!(
            app.world().get::<NodeStyle>(bar).unwrap().0.display,
            crate::style::Display::None
        );
        // Without reorder the drop is reported but nothing moves.
        app.world_mut().get_mut::<TreeView>(list).unwrap().reorder = false;
        app.world_mut().write_message(DragDropEvent::Dropped {
            source,
            target,
            payload: DragPayload::new(TREE_DRAG_KIND, UiValue::Index(0)),
        });
        app.update();
        let sent: Vec<TreeDropped> = app
            .world_mut()
            .resource_mut::<Messages<TreeDropped>>()
            .drain()
            .collect();
        assert!(!sent[0].moved);
    }

    #[test]
    fn separators_are_one_pixel_rules() {
        let mut world = World::new();
        let parent = world.spawn(UiNode).id();
        let mut commands = world.commands();
        let h = spawn_separator(&mut commands, parent, false);
        let v = spawn_separator(&mut commands, parent, true);
        world.flush();
        let size = |e: Entity| world.get::<NodeStyle>(e).unwrap().0.size;
        assert_eq!(size(h), [Dimension::Percent(100.0), Dimension::Points(1.0)]);
        assert_eq!(size(v), [Dimension::Points(1.0), Dimension::Percent(100.0)]);
        assert!(world.get::<Separator>(v).unwrap().vertical);
        assert_eq!(world.get::<ChildOf>(h).map(|c| c.0), Some(parent));
    }
}
