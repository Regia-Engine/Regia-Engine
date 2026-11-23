// SPDX-License-Identifier: MPL-2.0
//! Virtualised list v2 (ADR-273 lane 8, ADR-344): the data lives in the list (`ListItems` or
//! `ItemCount`), only the visible rows exist as entities, and a fixed pool of row entities is
//! recycled as the scroll offset moves.
//!
//! - **Geometry** ([`ListGeometry`]): uniform rows, variable row heights through a prefix-sum
//!   cache ([`RowMetrics`], fed by [`ListHeights`]) or a wrapped tile grid ([`ListTiles`]).
//! - **Item/entry split**: a row entity is an *entry*. Row `i` always lives in pool slot
//!   `i % pool_len`, so scrolling by one row rebinds exactly one entry. Each rebind writes a
//!   [`ListEntryBind`] message so a game can fill custom entry content ([`CustomEntries`]
//!   turns the default text writer off).
//! - **Selection** ([`ListSelection`]): none, single or multi, with Shift (range) and Ctrl
//!   (toggle) modifiers, keyboard and gamepad movement (focus follows the cursor) and
//!   type-ahead jump.
//! - **Sticky section headers** ([`ListHeaders`]) and [`ScrollToItem`].
//!
//! The old `ListView` + `ListItems` + `ItemCount` API is unchanged: a list with none of the
//! optional components behaves as before (uniform rows, no selection).
//!
//! The agent surface is `ui.list` and `ui.scroll_to` (`book/agents/ui-list.md`).

use std::collections::BTreeSet;
use std::ops::Range;

use regia_app::prelude::*;

use crate::focus::{FocusState, set_focus};
use crate::interaction::{UiButton, UiEvent, UiEventKind, UiModifiers};
use crate::layout::ScrollDirty;
use crate::node::{Classes, ComputedNode, Interactable, NodeStyle, Selected, UiNode, ZOffset};
use crate::scroll::{ItemCount, ListItems, ListView, ScrollOffset, ScrollVelocity};
use crate::style::{Dimension, Display, Edges, PositionType, Style};
use crate::text::{Text, TextSpan};
use crate::theme::Themed;

/// Sentinel for "this pool slot is bound to no item".
const UNBOUND: usize = usize::MAX;
/// How long a type-ahead prefix stays alive between key presses (game seconds).
pub const TYPE_AHEAD_TIMEOUT: f32 = 1.0;
/// Most rows `ui.list items` returns in one reply.
pub const MAX_ITEMS_PER_REPLY: usize = 500;

// ---------------------------------------------------------------------------------------------
// Geometry.
// ---------------------------------------------------------------------------------------------

/// Row tops and heights of a one-column list, in UI px.
///
/// Uniform lists need no table. Variable lists keep a prefix-sum table so the row at a scroll
/// offset is a binary search and a row's top is one lookup (10 000 rows cost one `Vec<f32>`).
#[derive(Debug, Clone, PartialEq)]
pub enum RowMetrics {
    /// Every row has the same height.
    Uniform {
        /// Row count.
        count: usize,
        /// Row height.
        height: f32,
    },
    /// Per-row heights; `prefix[i]` is the top of row `i` and `prefix[count]` the total height.
    Variable {
        /// Prefix sums, `count + 1` entries.
        prefix: Vec<f32>,
    },
}

impl RowMetrics {
    /// `count` rows of `height` each (heights below 1 px are raised to 1).
    pub fn uniform(count: usize, height: f32) -> Self {
        Self::Uniform {
            count,
            height: height.max(1.0),
        }
    }

    /// `count` rows; row `i` is `heights[i]` tall, or `default_height` when `heights` is shorter.
    pub fn variable(count: usize, default_height: f32, heights: &[f32]) -> Self {
        let default_height = default_height.max(1.0);
        let mut prefix = Vec::with_capacity(count + 1);
        let mut top = 0.0f32;
        prefix.push(top);
        for i in 0..count {
            top += heights.get(i).copied().unwrap_or(default_height).max(1.0);
            prefix.push(top);
        }
        Self::Variable { prefix }
    }

    /// Number of rows.
    pub fn count(&self) -> usize {
        match self {
            Self::Uniform { count, .. } => *count,
            Self::Variable { prefix } => prefix.len().saturating_sub(1),
        }
    }

    /// Height of all rows together.
    pub fn total_height(&self) -> f32 {
        match self {
            Self::Uniform { count, height } => *count as f32 * height,
            Self::Variable { prefix } => prefix.last().copied().unwrap_or(0.0),
        }
    }

    /// Top edge of row `index` (the total height for `index >= count`).
    pub fn top(&self, index: usize) -> f32 {
        match self {
            Self::Uniform { count, height } => index.min(*count) as f32 * height,
            Self::Variable { prefix } => prefix
                .get(index)
                .copied()
                .unwrap_or_else(|| prefix.last().copied().unwrap_or(0.0)),
        }
    }

    /// Height of row `index` (0 past the end).
    pub fn height(&self, index: usize) -> f32 {
        if index >= self.count() {
            return 0.0;
        }
        self.top(index + 1) - self.top(index)
    }

    /// The row that contains vertical position `y`, clamped to the first and last row (0 when empty).
    pub fn index_at(&self, y: f32) -> usize {
        let count = self.count();
        if count == 0 {
            return 0;
        }
        match self {
            Self::Uniform { height, .. } => ((y / height).floor().max(0.0) as usize).min(count - 1),
            Self::Variable { prefix } => {
                let passed = prefix.partition_point(|top| *top <= y);
                passed.saturating_sub(1).min(count - 1)
            }
        }
    }
}

/// A wrapped grid of equally sized tiles, in UI px.
#[derive(Debug, Clone, PartialEq)]
pub struct TileGrid {
    count: usize,
    columns: usize,
    tile: [f32; 2],
    gap: [f32; 2],
}

impl TileGrid {
    /// A grid of `count` tiles in a list `width` wide. `columns` fixes the column count; `None`
    /// fits as many whole tiles as the width allows (at least one).
    pub fn new(count: usize, width: f32, tile: [f32; 2], gap: [f32; 2], columns: Option<u32>) -> Self {
        let tile = [tile[0].max(1.0), tile[1].max(1.0)];
        let gap = [gap[0].max(0.0), gap[1].max(0.0)];
        let columns = match columns {
            Some(c) => c.max(1) as usize,
            None => (((width + gap[0]) / (tile[0] + gap[0])).floor().max(1.0)) as usize,
        };
        Self {
            count,
            columns,
            tile,
            gap,
        }
    }

    /// Tiles per row.
    pub fn columns(&self) -> usize {
        self.columns
    }

    /// Rows of tiles.
    pub fn rows(&self) -> usize {
        self.count.div_ceil(self.columns)
    }

    fn row_step(&self) -> f32 {
        self.tile[1] + self.gap[1]
    }

    /// Height of all tile rows together.
    pub fn total_height(&self) -> f32 {
        match self.rows() {
            0 => 0.0,
            rows => rows as f32 * self.row_step() - self.gap[1],
        }
    }

    /// `[x, y, w, h]` of tile `index`.
    pub fn item_rect(&self, index: usize) -> [f32; 4] {
        let (row, col) = (index / self.columns, index % self.columns);
        [
            col as f32 * (self.tile[0] + self.gap[0]),
            row as f32 * self.row_step(),
            self.tile[0],
            self.tile[1],
        ]
    }
}

/// Where the items of a list are: a column of rows or a grid of tiles.
#[derive(Debug, Clone, PartialEq)]
pub enum ListGeometry {
    /// One column of rows (uniform or variable height).
    Rows(RowMetrics),
    /// A wrapped grid of tiles.
    Tiles(TileGrid),
}

impl ListGeometry {
    /// Number of items.
    pub fn count(&self) -> usize {
        match self {
            Self::Rows(m) => m.count(),
            Self::Tiles(t) => t.count,
        }
    }

    /// Height of the whole content.
    pub fn total_height(&self) -> f32 {
        match self {
            Self::Rows(m) => m.total_height(),
            Self::Tiles(t) => t.total_height(),
        }
    }

    /// `[x, y, w, h]` of item `index` in a list `width` wide.
    pub fn item_rect(&self, index: usize, width: f32) -> [f32; 4] {
        match self {
            Self::Rows(m) => [0.0, m.top(index), width, m.height(index)],
            Self::Tiles(t) => t.item_rect(index),
        }
    }

    /// Items that intersect `[offset, offset + viewport]`, widened by `overscan` rows each side.
    pub fn visible_range(&self, offset: f32, viewport: f32, overscan: usize) -> Range<usize> {
        let count = self.count();
        if count == 0 {
            return 0..0;
        }
        match self {
            Self::Rows(m) => {
                let first = m.index_at(offset);
                let last = m.index_at(offset + viewport.max(0.0));
                first.saturating_sub(overscan)..(last + 1 + overscan).min(count)
            }
            Self::Tiles(t) => {
                let step = t.row_step();
                let first_row = (offset / step).floor().max(0.0) as usize;
                let last_row = ((offset + viewport.max(0.0)) / step).floor().max(0.0) as usize;
                let start = (first_row.saturating_sub(overscan) * t.columns).min(count);
                let end = ((last_row + 1 + overscan) * t.columns).min(count);
                start..end
            }
        }
    }

    /// Items one page (a viewport) holds; at least one row's worth.
    pub fn page_items(&self, offset: f32, viewport: f32) -> usize {
        match self {
            Self::Rows(m) => {
                let first = m.index_at(offset);
                let last = m.index_at(offset + viewport.max(0.0));
                (last - first).max(1)
            }
            Self::Tiles(t) => ((viewport / t.row_step()).floor().max(1.0) as usize) * t.columns,
        }
    }
}

/// Where an item lands in the viewport after a scroll-to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollAlign {
    /// Scroll as little as possible (nothing when the item is already fully visible).
    #[default]
    Nearest,
    /// The item's top at the viewport's top.
    Start,
    /// The item centered.
    Center,
    /// The item's bottom at the viewport's bottom.
    End,
}

impl ScrollAlign {
    /// Parses `"nearest"`, `"start"`, `"center"` or `"end"`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "nearest" => Some(Self::Nearest),
            "start" => Some(Self::Start),
            "center" => Some(Self::Center),
            "end" => Some(Self::End),
            _ => None,
        }
    }
}

/// The scroll offset (before clamping) that puts an item of `top`/`height` where `align` says.
pub fn offset_for_item(top: f32, height: f32, offset: f32, viewport: f32, align: ScrollAlign) -> f32 {
    match align {
        ScrollAlign::Start => top,
        ScrollAlign::End => top + height - viewport,
        ScrollAlign::Center => top + height * 0.5 - viewport * 0.5,
        ScrollAlign::Nearest => {
            if top < offset {
                top
            } else if top + height > offset + viewport {
                top + height - viewport
            } else {
                offset
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Selection and type-ahead.
// ---------------------------------------------------------------------------------------------

/// How many items a list lets the user select.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionMode {
    /// Nothing can be selected.
    None,
    /// At most one item.
    #[default]
    Single,
    /// Any number; Shift extends a range from the anchor, Ctrl toggles.
    Multi,
}

impl SelectionMode {
    /// Parses `"none"`, `"single"` or `"multi"`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "none" => Some(Self::None),
            "single" => Some(Self::Single),
            "multi" => Some(Self::Multi),
            _ => None,
        }
    }

    /// The lowercase name `parse` accepts.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Single => "single",
            Self::Multi => "multi",
        }
    }
}

/// The selection of a list: a set of item indices plus the anchor Shift ranges grow from.
/// Every mutator returns whether the set changed.
#[derive(Component, Debug, Clone, PartialEq, Eq, Default)]
pub struct ListSelection {
    /// What the user may select.
    pub mode: SelectionMode,
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
}

impl ListSelection {
    /// An empty selection of `mode`.
    pub fn new(mode: SelectionMode) -> Self {
        Self {
            mode,
            selected: BTreeSet::new(),
            anchor: None,
        }
    }

    /// Whether item `index` is selected.
    pub fn is_selected(&self, index: usize) -> bool {
        self.selected.contains(&index)
    }

    /// Selected indices in ascending order.
    pub fn to_vec(&self) -> Vec<usize> {
        self.selected.iter().copied().collect()
    }

    /// Number of selected items.
    pub fn len(&self) -> usize {
        self.selected.len()
    }

    /// Whether nothing is selected.
    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    /// The item Shift ranges extend from.
    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    /// Deselects everything.
    pub fn clear(&mut self) -> bool {
        self.anchor = None;
        let changed = !self.selected.is_empty();
        self.selected.clear();
        changed
    }

    /// Selects exactly `index` (and makes it the anchor).
    pub fn select_only(&mut self, index: usize) -> bool {
        if self.mode == SelectionMode::None {
            return false;
        }
        self.anchor = Some(index);
        if self.selected.len() == 1 && self.selected.contains(&index) {
            return false;
        }
        self.selected.clear();
        self.selected.insert(index);
        true
    }

    /// Flips `index` (and makes it the anchor). In single mode selecting replaces the old item.
    pub fn toggle(&mut self, index: usize) -> bool {
        match self.mode {
            SelectionMode::None => false,
            SelectionMode::Single => {
                if self.selected.contains(&index) {
                    self.anchor = None;
                    self.selected.remove(&index)
                } else {
                    self.select_only(index)
                }
            }
            SelectionMode::Multi => {
                self.anchor = Some(index);
                if !self.selected.remove(&index) {
                    self.selected.insert(index);
                }
                true
            }
        }
    }

    /// Selects `from..=to` (either order). `additive` keeps what was selected; otherwise the range
    /// replaces it. The anchor is left alone. Single mode selects `to` only.
    pub fn select_range(&mut self, from: usize, to: usize, additive: bool) -> bool {
        match self.mode {
            SelectionMode::None => false,
            SelectionMode::Single => {
                let anchor = self.anchor;
                let changed = self.select_only(to);
                self.anchor = anchor.or(Some(to));
                changed
            }
            SelectionMode::Multi => {
                let before = self.selected.clone();
                if !additive {
                    self.selected.clear();
                }
                self.selected.extend(from.min(to)..=from.max(to));
                self.selected != before
            }
        }
    }

    /// Selects items `0..count` (multi mode only).
    pub fn select_all(&mut self, count: usize) -> bool {
        if self.mode != SelectionMode::Multi {
            return false;
        }
        let all: BTreeSet<usize> = (0..count).collect();
        let changed = all != self.selected;
        self.selected = all;
        changed
    }

    /// Replaces the selection with `indices` (single mode keeps the last one).
    pub fn set(&mut self, indices: &[usize]) -> bool {
        let wanted: BTreeSet<usize> = match self.mode {
            SelectionMode::None => BTreeSet::new(),
            SelectionMode::Single => indices.last().copied().into_iter().collect(),
            SelectionMode::Multi => indices.iter().copied().collect(),
        };
        self.anchor = wanted.iter().next_back().copied();
        let changed = wanted != self.selected;
        self.selected = wanted;
        changed
    }

    /// Drops selected indices at or past `count` (the list shrank).
    pub fn retain_below(&mut self, count: usize) -> bool {
        let before = self.selected.len();
        self.selected.retain(|i| *i < count);
        if self.anchor.is_some_and(|a| a >= count) {
            self.anchor = None;
        }
        self.selected.len() != before
    }

    /// A pointer click on `index`: plain selects only it, Ctrl toggles it, Shift selects the range
    /// from the anchor (added to the selection when Ctrl is also held).
    pub fn click(&mut self, index: usize, shift: bool, ctrl: bool) -> bool {
        match self.mode {
            SelectionMode::None => false,
            SelectionMode::Single => {
                if ctrl {
                    self.toggle(index)
                } else {
                    self.select_only(index)
                }
            }
            SelectionMode::Multi => {
                if shift {
                    let anchor = self.anchor.unwrap_or(index);
                    self.select_range(anchor, index, ctrl)
                } else if ctrl {
                    self.toggle(index)
                } else {
                    self.select_only(index)
                }
            }
        }
    }

    /// The cursor moved to `index` by keyboard or gamepad: plain selects only it, Shift (multi)
    /// extends from the anchor, Ctrl (multi) moves without selecting.
    pub fn step(&mut self, index: usize, shift: bool, ctrl: bool) -> bool {
        match self.mode {
            SelectionMode::None => false,
            SelectionMode::Single => self.select_only(index),
            SelectionMode::Multi => {
                if shift {
                    let anchor = self.anchor.unwrap_or(index);
                    self.select_range(anchor, index, false)
                } else if ctrl {
                    false
                } else {
                    self.select_only(index)
                }
            }
        }
    }
}

/// Collects typed characters into a prefix that expires after [`TYPE_AHEAD_TIMEOUT`] seconds.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TypeAhead {
    buffer: String,
    last: f32,
}

impl TypeAhead {
    /// Adds `ch` typed at game time `now` (seconds) and returns the prefix so far.
    pub fn push(&mut self, ch: char, now: f32) -> &str {
        if now - self.last > TYPE_AHEAD_TIMEOUT {
            self.buffer.clear();
        }
        self.buffer.push(ch);
        self.last = now;
        &self.buffer
    }

    /// The prefix typed so far.
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// Forgets the prefix.
    pub fn clear(&mut self) {
        self.buffer.clear();
    }
}

/// The item a type-ahead `prefix` jumps to: the first label (case-insensitive) that starts with it,
/// searching from the cursor and wrapping. A one-character prefix starts after the cursor, so
/// repeating a letter cycles through items; a longer prefix may stay on the cursor item.
pub fn type_ahead_match(
    count: usize,
    cursor: Option<usize>,
    prefix: &str,
    label: impl Fn(usize) -> String,
) -> Option<usize> {
    if count == 0 || prefix.is_empty() {
        return None;
    }
    let prefix = prefix.to_lowercase();
    let start = match (cursor, prefix.chars().count()) {
        (Some(c), 1) => c + 1,
        (Some(c), _) => c,
        (None, _) => 0,
    };
    (0..count)
        .map(|step| (start + step) % count)
        .find(|i| label(*i).to_lowercase().starts_with(&prefix))
}

// ---------------------------------------------------------------------------------------------
// Sticky headers.
// ---------------------------------------------------------------------------------------------

/// Item indices that are section headers, ascending. The header of the section at the top of
/// the viewport sticks to the top until the next header pushes it out.
#[derive(Component, Debug, Clone, PartialEq, Eq, Default)]
pub struct ListHeaders(pub Vec<usize>);

impl ListHeaders {
    /// Whether `index` is a header.
    pub fn contains(&self, index: usize) -> bool {
        self.0.binary_search(&index).is_ok()
    }
}

/// The sticky header for scroll `offset`: the header item and the content-space `y` of its top.
/// Rows only (tile lists have no sticky headers). `headers` must be ascending.
pub fn sticky_position(
    headers: &[usize],
    geometry: &ListGeometry,
    offset: f32,
    width: f32,
) -> Option<(usize, f32)> {
    let ListGeometry::Rows(rows) = geometry else {
        return None;
    };
    if rows.count() == 0 {
        return None;
    }
    let at_top = rows.index_at(offset);
    let after = headers.partition_point(|h| *h <= at_top);
    let current = *headers.get(after.checked_sub(1)?)?;
    let header_h = geometry.item_rect(current, width)[3];
    let own_top = rows.top(current);
    let mut y = offset.max(own_top);
    if let Some(next) = headers.get(after) {
        let next_top = rows.top(*next);
        if next_top < y + header_h {
            y = (next_top - header_h).max(own_top);
        }
    }
    Some((current, y))
}

// ---------------------------------------------------------------------------------------------
// Components and messages.
// ---------------------------------------------------------------------------------------------

/// Per-item row heights (UI px) for a list with variable heights. Items past the end use
/// `ListView::item_height`. Changing it rebuilds the prefix-sum cache.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct ListHeights(pub Vec<f32>);

/// Tile (wrapped grid) mode: items become `tile_size` cells laid out left to right and wrapped.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct ListTiles {
    /// Tile width and height (UI px).
    pub tile_size: [f32; 2],
    /// Horizontal and vertical gap between tiles (UI px).
    pub gap: [f32; 2],
    /// Fixed column count; `None` fits as many tiles as the width allows.
    pub columns: Option<u32>,
}

impl Default for ListTiles {
    fn default() -> Self {
        Self {
            tile_size: [96.0, 96.0],
            gap: [8.0, 8.0],
            columns: None,
        }
    }
}

/// Marker: the list's rows come from a `repeat` (ADR-2715, `docs/specs/81-ui-designer.md` § 2.8).
/// The list keeps its geometry, scroll extent, scroll-to, sticky header and selection, but spawns
/// no row pool of its own: the repeat materializes the visible items as row entities that carry
/// [`ListRow`], and this list marks them [`Selected`] and focuses them.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RepeatRows;

/// Marker: the game fills row content itself from [`ListEntryBind`]; the default label writer
/// stays off.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CustomEntries;

/// Request: scroll the list so an item is in view. The virtualise system consumes it (the
/// component is removed the frame it is applied).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollToItem {
    /// The item to bring into view.
    pub index: usize,
    /// Where it lands.
    pub align: ScrollAlign,
}

/// On a row entity: the list it belongs to and the item it currently shows.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListRow {
    /// The list entity.
    pub list: Entity,
    /// The item index the row shows now (changes when the row is recycled).
    pub index: usize,
}

/// A row entity was (re)bound to an item. Custom entries fill their content in response.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListEntryBind {
    /// The list entity.
    pub list: Entity,
    /// The row entity.
    pub row: Entity,
    /// The item it now shows.
    pub index: usize,
}

/// The selection of a list changed.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ListSelectionChanged {
    /// The list entity.
    pub list: Entity,
    /// The selected indices, ascending.
    pub selected: Vec<usize>,
}

/// An item was activated (double-click).
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListActivated {
    /// The list entity.
    pub list: Entity,
    /// The activated item.
    pub index: usize,
}

/// Runtime state of a list (rows spawned, counters, keyboard cursor). Inserted automatically.
#[derive(Component, Debug, Clone, Default)]
pub struct ListState {
    spacer: Option<Entity>,
    sticky: Option<Entity>,
    bound: Vec<usize>,
    geometry: Option<ListGeometry>,
    geometry_key: (usize, u32),
    range: Range<usize>,
    spawned: u64,
    rebinds: u64,
    last_rebinds: u32,
    page: usize,
    total: usize,
    total_height: f32,
    cursor: Option<usize>,
    last_focus: Option<Entity>,
    pending_focus: Option<usize>,
    typeahead: TypeAhead,
}

impl ListState {
    fn bound_at(&self, slot: usize) -> usize {
        self.bound.get(slot).copied().unwrap_or(UNBOUND)
    }

    fn set_bound(&mut self, slot: usize, index: usize) {
        if let Some(b) = self.bound.get_mut(slot) {
            *b = index;
        }
    }

    /// Items currently covered by live rows.
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    /// Row entities spawned over the list's lifetime.
    pub fn spawned_rows(&self) -> u64 {
        self.spawned
    }

    /// Row rebinds over the list's lifetime (a row changed the item it shows).
    pub fn rebinds(&self) -> u64 {
        self.rebinds
    }

    /// Rebinds in the most recent virtualise pass.
    pub fn last_rebinds(&self) -> u32 {
        self.last_rebinds
    }

    /// The sticky header node, once one has been spawned.
    pub fn sticky_node(&self) -> Option<Entity> {
        self.sticky
    }

    /// The keyboard cursor item.
    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    /// Item count at the last virtualise pass.
    pub fn item_count(&self) -> usize {
        self.total
    }

    /// Content height (UI px) at the last virtualise pass.
    pub fn total_height(&self) -> f32 {
        self.total_height
    }

    /// The type-ahead prefix typed so far.
    pub fn type_ahead(&self) -> &str {
        self.typeahead.buffer()
    }

    /// Sets the keyboard cursor without moving selection or focus.
    pub fn set_cursor(&mut self, index: Option<usize>) {
        self.cursor = index;
    }
}

// ---------------------------------------------------------------------------------------------
// Virtualise.
// ---------------------------------------------------------------------------------------------

/// The item pool slot `slot` shows when `range` is live and the pool has `n` slots.
fn index_for_slot(range: &Range<usize>, slot: usize, n: usize) -> Option<usize> {
    if n == 0 || range.is_empty() {
        return None;
    }
    let index = range.start + (slot + n - range.start % n) % n;
    (index < range.end).then_some(index)
}

/// The absolute style that puts a row at `rect` (`[x, y, w, h]`, UI px).
pub(crate) fn abs_style(rect: [f32; 4]) -> Style {
    Style {
        position: PositionType::Absolute,
        inset: Edges {
            left: Dimension::Points(rect[0]),
            top: Dimension::Points(rect[1]),
            right: Dimension::Auto,
            bottom: Dimension::Auto,
        },
        size: [Dimension::Points(rect[2]), Dimension::Points(rect[3])],
        ..Default::default()
    }
}

/// The style of a row that shows nothing.
pub(crate) fn hidden_style() -> Style {
    Style {
        display: Display::None,
        position: PositionType::Absolute,
        ..Default::default()
    }
}

fn item_label(items: Option<&ListItems>, index: usize) -> String {
    items
        .and_then(|its| its.0.get(index))
        .map(|it| it.label.clone())
        .unwrap_or_else(|| format!("Item {index}"))
}

fn row_class(headers: Option<&ListHeaders>, index: usize) -> &'static str {
    if headers.is_some_and(|h| h.contains(index)) {
        "list-header"
    } else {
        "list-item"
    }
}

fn label_text(label: String) -> Text {
    Text {
        spans: vec![TextSpan::lit(label)],
        ..Default::default()
    }
}

/// The row entity that shows `index`, if it is live.
fn row_entity(list: &ListView, state: &ListState, index: usize) -> Option<Entity> {
    let n = list.pool.len();
    if n == 0 {
        return None;
    }
    let slot = index % n;
    (state.bound.get(slot) == Some(&index))
        .then(|| list.pool.get(slot).copied())
        .flatten()
}

/// The geometry of a list of `total` items `width` wide (UI px): tiles, variable heights or uniform
/// rows, as [`virtualise_lists`] builds it.
pub(crate) fn list_geometry(
    list: &ListView,
    total: usize,
    width: f32,
    heights: Option<&ListHeights>,
    tiles: Option<&ListTiles>,
) -> ListGeometry {
    let default_h = list.item_height.max(1.0);
    match (tiles, heights) {
        (Some(t), _) => ListGeometry::Tiles(TileGrid::new(total, width, t.tile_size, t.gap, t.columns)),
        (None, Some(h)) => ListGeometry::Rows(RowMetrics::variable(total, default_h, &h.0)),
        (None, None) => ListGeometry::Rows(RowMetrics::uniform(total, default_h)),
    }
}

/// The repeat row of a [`RepeatRows`] list that shows `index`.
fn repeat_row(children: Option<&Children>, rows: &Query<&ListRow>, index: usize) -> Option<Entity> {
    children?
        .iter()
        .find(|&child| rows.get(child).is_ok_and(|r| r.index == index))
}

/// `PostUpdate` (before layout): applies scroll-to requests, picks the live item range, resizes the
/// row pool, binds rows to items and positions them, and keeps the spacer (full content height)
/// and the sticky header node current.
pub fn virtualise_lists(
    mut commands: Commands,
    mut lists: Query<(
        Entity,
        &ComputedNode,
        &mut ListView,
        &mut ListState,
        Option<&ListItems>,
        Option<&ItemCount>,
        Has<RepeatRows>,
    )>,
    children_of: Query<&Children>,
    heights: Query<Ref<ListHeights>>,
    tiles: Query<&ListTiles>,
    headers: Query<&ListHeaders>,
    selections: Query<&ListSelection>,
    custom: Query<(), With<CustomEntries>>,
    requests: Query<&ScrollToItem>,
    mut scrolls: Query<(&mut ScrollOffset, Option<&mut ScrollVelocity>)>,
    mut rows: Query<(
        &mut NodeStyle,
        Option<&mut Text>,
        Option<&mut Classes>,
        Option<&ListRow>,
        Has<Selected>,
    )>,
    mut binds: MessageWriter<ListEntryBind>,
    mut scroll_dirty: ResMut<ScrollDirty>,
) {
    let _span = tracing::trace_span!("ui_list_virtualise").entered();
    for (entity, computed, mut list, mut state, items, count, external) in lists.iter_mut() {
        if computed.rect[2] <= 0.0 {
            continue;
        }
        let total = list.count(items, count);
        let scale = computed.scale.max(0.001);
        let width = computed.rect[2] / scale;
        let viewport = computed.rect[3] / scale;
        let default_h = list.item_height.max(1.0);

        // Geometry: tiles, cached prefix sums, or uniform rows.
        let tile_cfg = tiles.get(entity).ok();
        let height_cfg = heights.get(entity).ok();
        let key = (total, default_h.to_bits());
        let geometry = match (tile_cfg, height_cfg) {
            (Some(t), _) => ListGeometry::Tiles(TileGrid::new(total, width, t.tile_size, t.gap, t.columns)),
            (None, Some(h)) => {
                let reusable = !h.is_changed() && state.geometry_key == key;
                match state.geometry.take() {
                    Some(g @ ListGeometry::Rows(RowMetrics::Variable { .. })) if reusable => g,
                    _ => ListGeometry::Rows(RowMetrics::variable(total, default_h, &h.0)),
                }
            }
            (None, None) => ListGeometry::Rows(RowMetrics::uniform(total, default_h)),
        };
        state.geometry_key = key;
        state.total = total;
        state.total_height = geometry.total_height();

        // Scroll-to request.
        let mut offset = scrolls.get_mut(entity).map_or(0.0, |(o, _)| o.0[1]);
        if let Ok(request) = requests.get(entity) {
            if total > 0 {
                let rect = geometry.item_rect(request.index.min(total - 1), width);
                let wanted = offset_for_item(rect[1], rect[3], offset / scale, viewport, request.align);
                let max = (geometry.total_height() - viewport).max(0.0);
                offset = wanted.clamp(0.0, max) * scale;
                if let Ok((mut o, velocity)) = scrolls.get_mut(entity) {
                    let moved = o.0[1] != offset;
                    o.0[1] = offset;
                    if let Some(mut v) = velocity {
                        v.v = [0.0; 2];
                    }
                    // Row rebinds reach layout through `Changed<NodeStyle>`. A scroll-to that
                    // only moves the offset stays on the scroll-only walk (ADR-520).
                    if moved {
                        scroll_dirty.mark();
                    }
                }
            }
            commands.entity(entity).remove::<ScrollToItem>();
        }
        let offset_ui = offset / scale;
        let range = geometry.visible_range(offset_ui, viewport, list.overscan as usize);
        state.page = geometry.page_items(offset_ui, viewport);
        // A repeat-driven list spawns no pool: the repeat's rows are its entries.
        let need = if external { 0 } else { range.len() };

        // Pool size: grow when short, shrink when far too large.
        let old_n = list.pool.len();
        let new_n = if external {
            0
        } else if need > old_n {
            need
        } else if old_n > need * 2 + 4 {
            if need == 0 { 0 } else { need + 2 }
        } else {
            old_n
        };
        let selection = selections.get(entity).ok();
        let header_list = headers.get(entity).ok();
        let is_custom = custom.contains(entity);
        let mut rebinds_now = 0u32;
        if new_n != old_n || list.dirty || state.bound.len() != old_n {
            while list.pool.len() > new_n {
                if let Some(row) = list.pool.pop()
                    && let Ok(mut cmds) = commands.get_entity(row)
                {
                    cmds.despawn();
                }
            }
            state.bound.clear();
            state.bound.resize(new_n, UNBOUND);
            for slot in old_n.min(new_n)..new_n {
                let target = index_for_slot(&range, slot, new_n);
                let (style, label, class) = match target {
                    Some(i) => (
                        abs_style(geometry.item_rect(i, width)),
                        item_label(items, i),
                        row_class(header_list, i),
                    ),
                    None => (hidden_style(), String::new(), "list-item"),
                };
                let mut row = commands.spawn((
                    UiNode,
                    NodeStyle::new(style),
                    Classes::one(class),
                    Interactable::default(),
                    crate::focus::Focusable::default(),
                    Themed::default(),
                    label_text(if is_custom { String::new() } else { label }),
                    ChildOf(entity),
                    ListRow {
                        list: entity,
                        index: target.unwrap_or(UNBOUND),
                    },
                ));
                if let Some(i) = target {
                    if selection.is_some_and(|s| s.is_selected(i)) {
                        row.insert(Selected);
                    }
                    binds.write(ListEntryBind {
                        list: entity,
                        row: row.id(),
                        index: i,
                    });
                    rebinds_now += 1;
                }
                let id = row.id();
                list.pool.push(id);
                state.set_bound(slot, target.unwrap_or(UNBOUND));
                state.spawned += 1;
            }
            list.dirty = false;
        }
        let spawned_slots = old_n.min(new_n)..new_n;

        // Reconcile existing rows (rows spawned above are already bound).
        for slot in 0..list.pool.len() {
            if spawned_slots.contains(&slot) {
                continue;
            }
            let Some(row_e) = list.pool.get(slot).copied() else {
                continue;
            };
            let Ok((mut node_style, text, classes, row, was_selected)) = rows.get_mut(row_e) else {
                continue;
            };
            match index_for_slot(&range, slot, new_n) {
                None => {
                    if node_style.0.display != Display::None {
                        node_style.0 = hidden_style();
                    }
                    state.set_bound(slot, UNBOUND);
                    if row.is_none_or(|r| r.index != UNBOUND) {
                        commands.entity(row_e).insert(ListRow {
                            list: entity,
                            index: UNBOUND,
                        });
                    }
                    if was_selected {
                        commands.entity(row_e).remove::<Selected>();
                    }
                }
                Some(i) => {
                    let style = abs_style(geometry.item_rect(i, width));
                    if node_style.0 != style {
                        node_style.0 = style;
                    }
                    if state.bound_at(slot) != i {
                        if !is_custom && let Some(mut text) = text {
                            let next = label_text(item_label(items, i));
                            if *text != next {
                                *text = next;
                            }
                        }
                        if let Some(mut classes) = classes {
                            let class = row_class(header_list, i);
                            if classes.0.first().map(String::as_str) != Some(class) {
                                *classes = Classes::one(class);
                            }
                        }
                        if row.is_none_or(|r| r.index != i) {
                            commands.entity(row_e).insert(ListRow {
                                list: entity,
                                index: i,
                            });
                        }
                        binds.write(ListEntryBind {
                            list: entity,
                            row: row_e,
                            index: i,
                        });
                        state.set_bound(slot, i);
                        rebinds_now += 1;
                    }
                    let want = selection.is_some_and(|s| s.is_selected(i));
                    if want != was_selected {
                        if want {
                            commands.entity(row_e).insert(Selected);
                        } else {
                            commands.entity(row_e).remove::<Selected>();
                        }
                    }
                }
            }
        }
        // Repeat rows: only their selection mark is the list's.
        if external && let Ok(children) = children_of.get(entity) {
            for child in children.iter() {
                let Ok((_, _, _, Some(row), was_selected)) = rows.get_mut(child) else {
                    continue;
                };
                if row.list != entity {
                    continue;
                }
                let want = row.index != UNBOUND && selection.is_some_and(|s| s.is_selected(row.index));
                if want != was_selected {
                    if want {
                        commands.entity(child).insert(Selected);
                    } else {
                        commands.entity(child).remove::<Selected>();
                    }
                }
            }
        }
        state.rebinds += u64::from(rebinds_now);
        state.last_rebinds = rebinds_now;
        if list.first_visible != range.start {
            list.first_visible = range.start;
        }
        state.range = range;

        // Spacer: one pixel at the bottom of the content so the scroll extent is the full list.
        let spacer_style = abs_style([0.0, (state.total_height - 1.0).max(0.0), 1.0, 1.0]);
        let spacer = state.spacer;
        match spacer {
            None => {
                let id = commands
                    .spawn((UiNode, NodeStyle::new(spacer_style), ChildOf(entity)))
                    .id();
                state.spacer = Some(id);
            }
            Some(id) => {
                if let Ok((mut s, ..)) = rows.get_mut(id)
                    && s.0 != spacer_style
                {
                    s.0 = spacer_style;
                }
            }
        }

        // Sticky header.
        let sticky = header_list.and_then(|h| sticky_position(&h.0, &geometry, offset_ui, width));
        match (state.sticky, sticky) {
            (None, Some((header, y))) => {
                let rect = geometry.item_rect(header, width);
                let id = commands
                    .spawn((
                        UiNode,
                        NodeStyle::new(abs_style([0.0, y, rect[2], rect[3]])),
                        Classes(vec!["list-header".to_owned(), "list-sticky".to_owned()]),
                        Themed::default(),
                        label_text(item_label(items, header)),
                        ZOffset(1),
                        ChildOf(entity),
                    ))
                    .id();
                state.sticky = Some(id);
            }
            (Some(id), found) => {
                if let Ok((mut s, text, _, _, _)) = rows.get_mut(id) {
                    let style = match found {
                        Some((header, y)) => {
                            let rect = geometry.item_rect(header, width);
                            if let Some(mut text) = text {
                                let next = label_text(item_label(items, header));
                                if *text != next {
                                    *text = next;
                                }
                            }
                            abs_style([0.0, y, rect[2], rect[3]])
                        }
                        None => hidden_style(),
                    };
                    if s.0 != style {
                        s.0 = style;
                    }
                }
            }
            (None, None) => {}
        }
        state.geometry = Some(geometry);
    }
}

// ---------------------------------------------------------------------------------------------
// Input.
// ---------------------------------------------------------------------------------------------

/// `Update` (first): gives every list its [`ListState`].
pub fn ensure_list_state(lists: Query<Entity, (With<ListView>, Without<ListState>)>, mut commands: Commands) {
    for entity in &lists {
        commands.entity(entity).insert(ListState::default());
    }
}

/// Finds the list a node belongs to: `(list, Some(item))` for a row (or a child of one),
/// `(list, None)` for the list itself or a child of it that is not in a row.
fn find_list(
    start: Entity,
    rows: &Query<&ListRow>,
    parents: &Query<&ChildOf>,
    list_marks: &Query<(), With<ListView>>,
) -> Option<(Entity, Option<usize>)> {
    let mut current = start;
    for _ in 0..8 {
        if let Ok(row) = rows.get(current) {
            return Some((row.list, Some(row.index)));
        }
        if list_marks.contains(current) {
            return Some((current, None));
        }
        current = parents.get(current).ok()?.0;
    }
    None
}

fn key_flag(key: &str) -> Option<&'static str> {
    let lower = key.to_ascii_lowercase();
    if lower.starts_with("shift") {
        Some("shift")
    } else if lower.starts_with("control") || lower.starts_with("ctrl") {
        Some("ctrl")
    } else if lower.starts_with("alt") {
        Some("alt")
    } else if lower.starts_with("meta") || lower.starts_with("super") {
        Some("meta")
    } else {
        None
    }
}

/// Moves a list's keyboard cursor to `index`: selection step, scroll into view, focus the row.
fn move_cursor(
    entity: Entity,
    index: usize,
    shift: bool,
    ctrl: bool,
    state: &mut ListState,
    selection: Option<&mut ListSelection>,
    changed: &mut MessageWriter<ListSelectionChanged>,
    commands: &mut Commands,
) {
    state.cursor = Some(index);
    state.pending_focus = Some(index);
    if let Some(selection) = selection
        && selection.step(index, shift, ctrl)
    {
        changed.write(ListSelectionChanged {
            list: entity,
            selected: selection.to_vec(),
        });
    }
    commands.entity(entity).insert(ScrollToItem {
        index,
        align: ScrollAlign::Nearest,
    });
}

/// `Update` (after focus navigation): pointer selection, keyboard and gamepad cursor movement,
/// type-ahead, and trimming a selection after the list shrank.
pub fn ui_list_input(
    time: Res<Time>,
    mut events: MessageReader<UiEvent>,
    mut keys: MessageReader<regia_platform::KeyboardInput>,
    mut mods: Local<UiModifiers>,
    focus: Res<FocusState>,
    row_of: Query<&ListRow>,
    parents: Query<&ChildOf>,
    list_marks: Query<(), With<ListView>>,
    mut lists: Query<(
        Entity,
        &mut ListState,
        Option<&mut ListSelection>,
        Option<&ListItems>,
    )>,
    mut changed: MessageWriter<ListSelectionChanged>,
    mut activated: MessageWriter<ListActivated>,
    mut commands: Commands,
) {
    let _span = tracing::trace_span!("ui_list_input").entered();
    // Selection that outlived its items.
    for (entity, state, selection, _) in &mut lists {
        if let Some(mut selection) = selection
            && state.total > 0
            && selection.retain_below(state.total)
        {
            changed.write(ListSelectionChanged {
                list: entity,
                selected: selection.to_vec(),
            });
        }
    }
    // Pointer.
    for event in events.read() {
        let is_click = match event.kind {
            UiEventKind::Click => true,
            UiEventKind::DoubleClick => false,
            _ => continue,
        };
        if event.button != UiButton::Primary {
            continue;
        }
        let Some((list, Some(index))) = find_list(event.target, &row_of, &parents, &list_marks) else {
            continue;
        };
        let Ok((_, mut state, selection, _)) = lists.get_mut(list) else {
            continue;
        };
        if is_click {
            state.cursor = Some(index);
            if let Some(mut selection) = selection
                && selection.click(index, event.modifiers.shift, event.modifiers.ctrl)
            {
                changed.write(ListSelectionChanged {
                    list,
                    selected: selection.to_vec(),
                });
            }
        } else {
            activated.write(ListActivated { list, index });
        }
    }
    // Keyboard.
    let now = time.elapsed_secs();
    for key in keys.read() {
        let pressed = key.state == regia_platform::ButtonState::Pressed;
        if let Some(flag) = key_flag(&key.logical_key).or_else(|| key_flag(&key.physical_key)) {
            match flag {
                "shift" => mods.shift = pressed,
                "ctrl" => mods.ctrl = pressed,
                "alt" => mods.alt = pressed,
                _ => mods.meta = pressed,
            }
            continue;
        }
        if !pressed {
            continue;
        }
        let Some(focused) = focus.focused else { continue };
        let Some((list, row_index)) = find_list(focused, &row_of, &parents, &list_marks) else {
            continue;
        };
        let Ok((_, mut state, mut selection, items)) = lists.get_mut(list) else {
            continue;
        };
        let total = state.total;
        if total == 0 {
            continue;
        }
        let base = state.cursor.or(row_index).unwrap_or(0).min(total - 1);
        let page = state.page.max(1);
        let name = key.logical_key.to_ascii_lowercase();
        let target = match name.as_str() {
            "home" => Some(0),
            "end" => Some(total - 1),
            "pageup" => Some(base.saturating_sub(page)),
            "pagedown" => Some((base + page).min(total - 1)),
            _ => None,
        };
        if let Some(target) = target {
            let (shift, ctrl) = (mods.shift, mods.ctrl);
            move_cursor(
                list,
                target,
                shift,
                ctrl,
                &mut state,
                selection.as_deref_mut(),
                &mut changed,
                &mut commands,
            );
            continue;
        }
        let mut chars = key.logical_key.chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else {
            continue;
        };
        if mods.ctrl && ch.eq_ignore_ascii_case(&'a') {
            if let Some(selection) = selection.as_deref_mut()
                && selection.select_all(total)
            {
                changed.write(ListSelectionChanged {
                    list,
                    selected: selection.to_vec(),
                });
            }
            continue;
        }
        if ch.is_control()
            || mods.ctrl
            || mods.alt
            || mods.meta
            || (ch.is_whitespace() && state.typeahead.buffer().is_empty())
        {
            continue;
        }
        let prefix = state.typeahead.push(ch, now).to_owned();
        let found = type_ahead_match(total, state.cursor.or(row_index), &prefix, |i| {
            item_label(items, i)
        });
        if let Some(found) = found {
            move_cursor(
                list,
                found,
                false,
                false,
                &mut state,
                selection.as_deref_mut(),
                &mut changed,
                &mut commands,
            );
        }
    }
    // Focus moved onto a row (arrow keys, gamepad, Tab): the cursor follows.
    for (_, mut state, _, _) in &mut lists {
        if state.last_focus.is_some() && state.last_focus != focus.focused {
            state.last_focus = None;
        }
    }
    if let Some(focused) = focus.focused
        && let Some((list, Some(index))) = find_list(focused, &row_of, &parents, &list_marks)
        && let Ok((_, mut state, mut selection, _)) = lists.get_mut(list)
        && state.last_focus != Some(focused)
    {
        state.last_focus = Some(focused);
        if state.cursor != Some(index) {
            let (shift, ctrl) = (mods.shift, mods.ctrl);
            move_cursor(
                list,
                index,
                shift,
                ctrl,
                &mut state,
                selection.as_deref_mut(),
                &mut changed,
                &mut commands,
            );
            state.pending_focus = None;
        }
    }
}

/// `Update` (after [`ui_list_input`]): focuses the row a cursor jump asked for once it is live, and
/// returns focus to the cursor's row when recycling moved the focused row to another item.
pub fn ui_list_focus(
    mut focus: ResMut<FocusState>,
    mut lists: Query<(&ListView, &mut ListState, Option<&Children>, Has<RepeatRows>)>,
    rows: Query<&ListRow>,
    mut commands: Commands,
    mut events: MessageWriter<UiEvent>,
) {
    let _span = tracing::debug_span!("ui_list_focus").entered();
    for (list, mut state, children, external) in &mut lists {
        if let Some(f) = focus.focused
            && state.last_focus == Some(f)
            && let Ok(row) = rows.get(f)
            && let Some(cursor) = state.cursor
            && row.index != cursor
        {
            state.pending_focus = Some(cursor);
        }
        if let Some(index) = state.pending_focus
            && let Some(row) = if external {
                repeat_row(children, &rows, index)
            } else {
                row_entity(list, &state, index)
            }
        {
            set_focus(Some(row), &mut focus, &mut commands, &mut events);
            state.last_focus = Some(row);
            state.pending_focus = None;
        }
    }
}

/// Installs the list v2 systems and messages. [`crate::UiPlugin`] adds it.
pub struct ListPlugin;

impl Plugin for ListPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<ListEntryBind>()
            .add_message::<ListSelectionChanged>()
            .add_message::<ListActivated>()
            .add_systems(
                Update,
                (ensure_list_state, ui_list_input, ui_list_focus)
                    .chain()
                    .after(crate::focus::ui_focus_navigation)
                    .before(crate::scroll::ui_scroll),
            );
    }
}

// ---------------------------------------------------------------------------------------------
// Programmatic API (the agent surface uses these).
// ---------------------------------------------------------------------------------------------

/// Why a list call failed. [`ListError::code`] is the stable agent error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListError {
    /// The entity does not exist or is not a list.
    NotAList,
    /// The index is past the last item.
    IndexOutOfRange,
    /// The list has no `ListSelection` (or its mode is `none`).
    NoSelection,
}

impl ListError {
    /// Stable lowercase code.
    pub fn code(self) -> &'static str {
        match self {
            Self::NotAList => "not_a_list",
            Self::IndexOutOfRange => "index_out_of_range",
            Self::NoSelection => "no_selection",
        }
    }
}

/// A snapshot of one list for `ui.list`.
#[derive(Debug, Clone, PartialEq)]
pub struct ListInfo {
    /// Current item count ([`ListView::count`] of the list's items). This is not the count
    /// from the last virtualise pass.
    pub count: usize,
    /// Half-open window of items that have live rows. Empty until a virtualise pass lays the
    /// list out (a list whose width is still 0 is skipped).
    pub range: Range<usize>,
    /// Live row entities.
    pub live_rows: usize,
    /// Row entities spawned so far.
    pub spawned_rows: u64,
    /// Row rebinds so far.
    pub rebinds: u64,
    /// Content height (UI px) of [`Self::count`]. Tile columns use the laid-out width; a list
    /// that has not been laid out uses one column.
    pub total_height: f32,
    /// Scroll offset (physical px).
    pub offset: f32,
    /// `"rows"`, `"variable_rows"` or `"tiles"`.
    pub layout: &'static str,
    /// Selection mode name, or `"none"` without a [`ListSelection`].
    pub selection_mode: &'static str,
    /// Selected indices.
    pub selected: Vec<usize>,
    /// Keyboard cursor.
    pub cursor: Option<usize>,
    /// Section header items.
    pub headers: Vec<usize>,
}

/// Reads a list. `None` when `entity` is not a list.
pub fn list_info(world: &World, entity: Entity) -> Option<ListInfo> {
    let list = world.get::<ListView>(entity)?;
    let state = world.get::<ListState>(entity).cloned().unwrap_or_default();
    let items = world.get::<ListItems>(entity);
    let total = list.count(items, world.get::<ItemCount>(entity));
    // Tile columns need the laid-out width. Uniform and variable rows do not. A missing or
    // zero-width node reports one column for an auto-column tile list.
    let width = world.get::<ComputedNode>(entity).map_or(0.0, |computed| {
        let scale = computed.scale.max(0.001);
        if computed.rect[2] > 0.0 {
            computed.rect[2] / scale
        } else {
            0.0
        }
    });
    let total_height = list_geometry(
        list,
        total,
        width,
        world.get::<ListHeights>(entity),
        world.get::<ListTiles>(entity),
    )
    .total_height();
    let selection = world.get::<ListSelection>(entity);
    let layout = if world.get::<ListTiles>(entity).is_some() {
        "tiles"
    } else if world.get::<ListHeights>(entity).is_some() {
        "variable_rows"
    } else {
        "rows"
    };
    Some(ListInfo {
        count: total,
        range: state.range(),
        live_rows: crate::scroll::live_rows(list),
        spawned_rows: state.spawned,
        rebinds: state.rebinds,
        total_height,
        offset: world.get::<ScrollOffset>(entity).map_or(0.0, |o| o.0[1]),
        layout,
        selection_mode: selection.map_or("none", |s| s.mode.name()),
        selected: selection.map(ListSelection::to_vec).unwrap_or_default(),
        cursor: state.cursor,
        headers: world
            .get::<ListHeaders>(entity)
            .map(|h| h.0.clone())
            .unwrap_or_default(),
    })
}

/// `(index, label, selected)` of up to `limit` items starting at `start`.
pub fn list_items(
    world: &World,
    entity: Entity,
    start: usize,
    limit: usize,
) -> Option<Vec<(usize, String, bool)>> {
    let list = world.get::<ListView>(entity)?;
    let items = world.get::<ListItems>(entity);
    let total = list.count(items, world.get::<ItemCount>(entity));
    let selection = world.get::<ListSelection>(entity);
    let end = total.min(start.saturating_add(limit.min(MAX_ITEMS_PER_REPLY)));
    Some(
        (start.min(end)..end)
            .map(|i| {
                (
                    i,
                    item_label(items, i),
                    selection.is_some_and(|s| s.is_selected(i)),
                )
            })
            .collect(),
    )
}

/// How [`select_items`] combines `indices` with the current selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectOp {
    /// Replace the selection.
    Set,
    /// Add to it.
    Add,
    /// Flip each index.
    Toggle,
    /// Select the range between the first and last index.
    Range,
    /// Select everything (multi mode).
    All,
    /// Select nothing.
    Clear,
}

impl SelectOp {
    /// Parses `"set"`, `"add"`, `"toggle"`, `"range"`, `"all"` or `"clear"`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "set" => Some(Self::Set),
            "add" => Some(Self::Add),
            "toggle" => Some(Self::Toggle),
            "range" => Some(Self::Range),
            "all" => Some(Self::All),
            "clear" => Some(Self::Clear),
            _ => None,
        }
    }
}

/// Changes a list's selection and returns the new selected indices. Sends
/// [`ListSelectionChanged`] when the set changed.
pub fn select_items(
    world: &mut World,
    entity: Entity,
    op: SelectOp,
    indices: &[usize],
) -> Result<Vec<usize>, ListError> {
    let (total, mode) = {
        let list = world.get::<ListView>(entity).ok_or(ListError::NotAList)?;
        let total = list.count(world.get::<ListItems>(entity), world.get::<ItemCount>(entity));
        let mode = world
            .get::<ListSelection>(entity)
            .map_or(SelectionMode::None, |s| s.mode);
        (total, mode)
    };
    if mode == SelectionMode::None {
        return Err(ListError::NoSelection);
    }
    if indices.iter().any(|i| *i >= total) {
        return Err(ListError::IndexOutOfRange);
    }
    let (changed, selected) = {
        let mut selection = world
            .get_mut::<ListSelection>(entity)
            .ok_or(ListError::NoSelection)?;
        let changed = match op {
            SelectOp::Set => selection.set(indices),
            SelectOp::Add => {
                let mut all = selection.to_vec();
                all.extend_from_slice(indices);
                selection.set(&all)
            }
            SelectOp::Toggle => indices.iter().fold(false, |acc, i| acc | selection.toggle(*i)),
            SelectOp::Range => match (indices.first(), indices.last()) {
                (Some(a), Some(b)) => selection.select_range(*a, *b, false),
                _ => false,
            },
            SelectOp::All => selection.select_all(total),
            SelectOp::Clear => selection.clear(),
        };
        (changed, selection.to_vec())
    };
    if let Some(mut state) = world.get_mut::<ListState>(entity) {
        state.cursor = indices.last().copied().or(state.cursor);
    }
    if changed {
        let _ = world.write_message(ListSelectionChanged {
            list: entity,
            selected: selected.clone(),
        });
    }
    Ok(selected)
}

/// Asks a list to scroll so `index` is in view (applied in the next `PostUpdate`).
pub fn request_scroll_to_item(
    world: &mut World,
    entity: Entity,
    index: usize,
    align: ScrollAlign,
) -> Result<(), ListError> {
    let list = world.get::<ListView>(entity).ok_or(ListError::NotAList)?;
    let total = list.count(world.get::<ListItems>(entity), world.get::<ItemCount>(entity));
    if index >= total {
        return Err(ListError::IndexOutOfRange);
    }
    world.entity_mut(entity).insert(ScrollToItem { index, align });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_metrics() {
        let m = RowMetrics::uniform(100, 20.0);
        assert_eq!(m.count(), 100);
        assert_eq!(m.total_height(), 2000.0);
        assert_eq!(m.top(5), 100.0);
        assert_eq!(m.height(5), 20.0);
        assert_eq!(m.index_at(99.0), 4);
        assert_eq!(m.index_at(100.0), 5);
        assert_eq!(m.index_at(-30.0), 0);
        assert_eq!(m.index_at(1.0e9), 99);
        assert_eq!(RowMetrics::uniform(0, 20.0).index_at(10.0), 0);
    }

    #[test]
    fn variable_metrics_match_prefix_sums() {
        let m = RowMetrics::variable(4, 10.0, &[10.0, 30.0, 20.0]);
        // Row 3 falls back to the default height.
        assert_eq!(m.total_height(), 70.0);
        assert_eq!(
            [m.top(0), m.top(1), m.top(2), m.top(3), m.top(4)],
            [0.0, 10.0, 40.0, 60.0, 70.0]
        );
        assert_eq!(m.height(1), 30.0);
        assert_eq!(m.index_at(0.0), 0);
        assert_eq!(m.index_at(9.9), 0);
        assert_eq!(m.index_at(10.0), 1);
        assert_eq!(m.index_at(39.9), 1);
        assert_eq!(m.index_at(40.0), 2);
        assert_eq!(m.index_at(65.0), 3);
        assert_eq!(m.index_at(500.0), 3);
        assert_eq!(m.height(9), 0.0);
    }

    #[test]
    fn variable_heights_below_one_are_raised() {
        let m = RowMetrics::variable(2, 10.0, &[0.0, -5.0]);
        assert_eq!(m.total_height(), 2.0);
    }

    #[test]
    fn visible_range_adds_overscan_and_clamps() {
        let g = ListGeometry::Rows(RowMetrics::uniform(100, 10.0));
        assert_eq!(g.visible_range(0.0, 50.0, 2), 0..8);
        assert_eq!(g.visible_range(500.0, 50.0, 2), 48..58);
        assert_eq!(g.visible_range(100_000.0, 50.0, 2), 97..100);
        assert_eq!(
            ListGeometry::Rows(RowMetrics::uniform(0, 10.0)).visible_range(0.0, 50.0, 2),
            0..0
        );
    }

    #[test]
    fn ten_thousand_rows_stay_bounded() {
        let heights: Vec<f32> = (0..10_000).map(|i| 20.0 + (i % 5) as f32 * 4.0).collect();
        let g = ListGeometry::Rows(RowMetrics::variable(10_000, 20.0, &heights));
        for offset in [0.0, 1234.5, 100_000.0, g.total_height()] {
            let r = g.visible_range(offset, 400.0, 2);
            assert!(r.len() <= 400 / 20 + 1 + 4, "range {r:?} at {offset}");
        }
    }

    #[test]
    fn tile_grid_wraps() {
        let t = TileGrid::new(10, 330.0, [100.0, 50.0], [10.0, 5.0], None);
        assert_eq!(t.columns(), 3);
        assert_eq!(t.rows(), 4);
        assert_eq!(t.total_height(), 4.0 * 55.0 - 5.0);
        assert_eq!(t.item_rect(0), [0.0, 0.0, 100.0, 50.0]);
        assert_eq!(t.item_rect(4), [110.0, 55.0, 100.0, 50.0]);
        let narrow = TileGrid::new(3, 10.0, [100.0, 50.0], [10.0, 5.0], None);
        assert_eq!(narrow.columns(), 1);
        let fixed = TileGrid::new(9, 10.0, [100.0, 50.0], [0.0, 0.0], Some(4));
        assert_eq!(fixed.columns(), 4);
        assert_eq!(
            TileGrid::new(0, 100.0, [10.0, 10.0], [1.0, 1.0], None).total_height(),
            0.0
        );
    }

    #[test]
    fn tile_visible_range_is_whole_rows() {
        let g = ListGeometry::Tiles(TileGrid::new(100, 330.0, [100.0, 50.0], [10.0, 5.0], None));
        // 3 columns, row step 55. Rows 2..=4 touch offset 110..220; overscan 1 -> rows 1..=5.
        assert_eq!(g.visible_range(110.0, 110.0, 1), 3..18);
        assert_eq!(g.visible_range(0.0, 10.0, 0), 0..3);
        assert_eq!(g.page_items(0.0, 110.0), 6);
    }

    #[test]
    fn offset_for_item_alignments() {
        // Item at 100..120, viewport 50 tall at offset 0.
        assert_eq!(
            offset_for_item(100.0, 20.0, 0.0, 50.0, ScrollAlign::Nearest),
            70.0
        );
        assert_eq!(
            offset_for_item(100.0, 20.0, 90.0, 50.0, ScrollAlign::Nearest),
            90.0
        );
        assert_eq!(
            offset_for_item(100.0, 20.0, 150.0, 50.0, ScrollAlign::Nearest),
            100.0
        );
        assert_eq!(offset_for_item(100.0, 20.0, 0.0, 50.0, ScrollAlign::Start), 100.0);
        assert_eq!(offset_for_item(100.0, 20.0, 0.0, 50.0, ScrollAlign::End), 70.0);
        assert_eq!(offset_for_item(100.0, 20.0, 0.0, 50.0, ScrollAlign::Center), 85.0);
    }

    #[test]
    fn slot_mapping_is_a_bijection_on_the_range() {
        let range = 7..12;
        let n = 6;
        let mut seen = BTreeSet::new();
        for slot in 0..n {
            if let Some(i) = index_for_slot(&range, slot, n) {
                assert_eq!(i % n, slot);
                assert!(range.contains(&i));
                assert!(seen.insert(i));
            }
        }
        assert_eq!(seen.len(), range.len());
        assert_eq!(index_for_slot(&(0..0), 0, 4), None);
        assert_eq!(index_for_slot(&range, 0, 0), None);
    }

    #[test]
    fn scrolling_one_row_rebinds_one_slot() {
        let n = 8;
        let before = 10..18;
        let after = 11..19;
        let rebinds = (0..n)
            .filter(|slot| index_for_slot(&before, *slot, n) != index_for_slot(&after, *slot, n))
            .count();
        assert_eq!(rebinds, 1);
    }

    #[test]
    fn single_selection() {
        let mut s = ListSelection::new(SelectionMode::Single);
        assert!(s.click(3, false, false));
        assert!(!s.click(3, false, false));
        assert!(s.click(5, true, false));
        assert_eq!(s.to_vec(), vec![5]);
        assert!(s.click(5, false, true));
        assert!(s.is_empty());
        assert!(s.step(2, true, true));
        assert_eq!(s.to_vec(), vec![2]);
    }

    #[test]
    fn multi_selection_shift_and_ctrl() {
        let mut s = ListSelection::new(SelectionMode::Multi);
        s.click(2, false, false);
        s.click(5, true, false);
        assert_eq!(s.to_vec(), vec![2, 3, 4, 5]);
        // A second shift click replaces the range, anchored at 2.
        s.click(3, true, false);
        assert_eq!(s.to_vec(), vec![2, 3]);
        // Ctrl toggles and moves the anchor.
        s.click(8, false, true);
        assert_eq!(s.to_vec(), vec![2, 3, 8]);
        s.click(2, false, true);
        assert_eq!(s.to_vec(), vec![3, 8]);
        // Ctrl+Shift adds a range.
        s.click(10, true, true);
        assert_eq!(s.to_vec(), (2..=10).collect::<Vec<_>>());
        s.click(0, false, false);
        assert_eq!(s.to_vec(), vec![0]);
    }

    #[test]
    fn multi_keyboard_step() {
        let mut s = ListSelection::new(SelectionMode::Multi);
        assert!(s.step(4, false, false));
        assert!(s.step(6, true, false));
        assert_eq!(s.to_vec(), vec![4, 5, 6]);
        // Ctrl moves without touching the selection.
        assert!(!s.step(9, false, true));
        assert_eq!(s.to_vec(), vec![4, 5, 6]);
        assert!(s.step(2, true, false));
        assert_eq!(s.to_vec(), vec![2, 3, 4]);
    }

    #[test]
    fn none_mode_ignores_everything() {
        let mut s = ListSelection::new(SelectionMode::None);
        assert!(!s.click(1, false, false));
        assert!(!s.select_only(1));
        assert!(!s.select_all(5));
        assert!(!s.set(&[1, 2]));
        assert!(s.is_empty());
    }

    #[test]
    fn select_all_clear_and_retain() {
        let mut s = ListSelection::new(SelectionMode::Multi);
        assert!(s.select_all(5));
        assert_eq!(s.len(), 5);
        assert!(s.retain_below(3));
        assert_eq!(s.to_vec(), vec![0, 1, 2]);
        assert!(s.clear());
        assert!(!s.clear());
        let mut single = ListSelection::new(SelectionMode::Single);
        assert!(!single.select_all(5));
        assert!(single.set(&[1, 4]));
        assert_eq!(single.to_vec(), vec![4]);
    }

    #[test]
    fn type_ahead_buffer_expires() {
        let mut t = TypeAhead::default();
        assert_eq!(t.push('a', 0.2), "a");
        assert_eq!(t.push('b', 0.7), "ab");
        // More than the timeout since the last key: a new prefix starts.
        assert_eq!(t.push('c', 0.7 + TYPE_AHEAD_TIMEOUT + 0.1), "c");
        t.clear();
        assert_eq!(t.buffer(), "");
    }

    #[test]
    fn type_ahead_jumps_and_wraps() {
        let names = ["Apple", "avocado", "Banana", "Blueberry", "Cherry"];
        let label = |i: usize| names[i].to_owned();
        assert_eq!(type_ahead_match(5, None, "b", label), Some(2));
        // A single repeated letter cycles.
        assert_eq!(type_ahead_match(5, Some(2), "b", label), Some(3));
        assert_eq!(type_ahead_match(5, Some(3), "b", label), Some(2));
        // A longer prefix may stay on the cursor item.
        assert_eq!(type_ahead_match(5, Some(3), "bl", label), Some(3));
        assert_eq!(type_ahead_match(5, Some(4), "a", label), Some(0));
        assert_eq!(type_ahead_match(5, None, "z", label), None);
        assert_eq!(type_ahead_match(0, None, "a", label), None);
        assert_eq!(type_ahead_match(5, None, "", label), None);
    }

    #[test]
    fn sticky_header_follows_section_and_gets_pushed_out() {
        // Rows of 10px; headers at items 0 and 5 (tops 0 and 50).
        let g = ListGeometry::Rows(RowMetrics::uniform(10, 10.0));
        let headers = [0, 5];
        assert_eq!(sticky_position(&headers, &g, 0.0, 100.0), Some((0, 0.0)));
        assert_eq!(sticky_position(&headers, &g, 20.0, 100.0), Some((0, 20.0)));
        assert_eq!(sticky_position(&headers, &g, 35.0, 100.0), Some((0, 35.0)));
        // Next header (top 50) approaches: 50 < 45 + 10, so the sticky one is pushed up to 40.
        assert_eq!(sticky_position(&headers, &g, 45.0, 100.0), Some((0, 40.0)));
        // Once the next header reaches the top, it takes over.
        assert_eq!(sticky_position(&headers, &g, 50.0, 100.0), Some((5, 50.0)));
        assert_eq!(sticky_position(&headers, &g, 80.0, 100.0), Some((5, 80.0)));
        // No header at or above the top item.
        assert_eq!(sticky_position(&[3], &g, 0.0, 100.0), None);
        assert_eq!(sticky_position(&[], &g, 0.0, 100.0), None);
        let tiles = ListGeometry::Tiles(TileGrid::new(10, 100.0, [10.0, 10.0], [0.0, 0.0], None));
        assert_eq!(sticky_position(&headers, &tiles, 0.0, 100.0), None);
    }

    #[test]
    fn headers_lookup() {
        let h = ListHeaders(vec![0, 4, 9]);
        assert!(h.contains(4));
        assert!(!h.contains(5));
    }

    #[test]
    fn parse_names_round_trip() {
        for mode in [SelectionMode::None, SelectionMode::Single, SelectionMode::Multi] {
            assert_eq!(SelectionMode::parse(mode.name()), Some(mode));
        }
        assert_eq!(SelectionMode::parse("many"), None);
        assert_eq!(ScrollAlign::parse("center"), Some(ScrollAlign::Center));
        assert_eq!(ScrollAlign::parse("middle"), None);
        assert_eq!(SelectOp::parse("toggle"), Some(SelectOp::Toggle));
        assert_eq!(SelectOp::parse("flip"), None);
    }
}
