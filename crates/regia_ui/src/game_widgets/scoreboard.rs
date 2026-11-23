// SPDX-License-Identifier: MPL-2.0
//! The scoreboard (ADR-2958). A [`Scoreboard`] node shows a [`ScoreTable`]: one column of names and
//! one column per [`ScoreColumn`], sorted by a column, optionally grouped by team with team totals.
//! `regia_framework::game_ui` fills the table from `GameRules::scoreboard`; a game without the
//! framework writes [`Scoreboard::table`] itself. Clicking a heading sorts by it; again flips the
//! direction. With [`Scoreboard::hold_action`] the board shows only while that input action is held.

use std::cmp::Ordering;

use regia_app::prelude::*;
use regia_ecs::ecs::message::MessageCursor;
use regia_input::Actions;

use super::{
    line, literal_line, put_state, set_if_changed, set_shown, spawn_button, spawn_child, take_state,
    theme_colors,
};
use crate::color::UiColor;
use crate::interaction::{UiEvent, UiEventKind};
use crate::node::{Background, BackgroundKind, Border, NodeName, NodeStyle, PickThrough};
use crate::style::{Align, Dimension, Edges, FlexDirection, JustifyContent, Style};
use crate::text::TextAlign;
use crate::theme::ThemeColors;
use crate::widgets::VirtualButtons;

/// Most rows one board draws, team headings included.
pub const MAX_SCORE_ROWS: usize = 128;
/// Most score columns one board draws.
pub const MAX_SCORE_COLUMNS: usize = 8;
/// Width of a score column in UI px when it names none.
pub const DEFAULT_COLUMN_PX: f32 = 72.0;

/// A cell's value: numbers sort numerically and team totals add them up.
#[derive(Debug, Clone, PartialEq)]
pub enum ScoreValue {
    /// A number.
    Int(i64),
    /// Text (a ping label, a class name).
    Text(String),
}

impl Default for ScoreValue {
    fn default() -> Self {
        Self::Int(0)
    }
}

impl ScoreValue {
    /// As shown in a cell.
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Self::Int(v) => v.to_string(),
            Self::Text(t) => t.clone(),
        }
    }

    fn compare(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => a.cmp(b),
            (Self::Text(a), Self::Text(b)) => a.cmp(b),
            // Numbers before text in an ascending sort, so a mixed column has one order.
            (Self::Int(_), Self::Text(_)) => Ordering::Less,
            (Self::Text(_), Self::Int(_)) => Ordering::Greater,
        }
    }
}

/// A score column.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScoreColumn {
    /// Stable key (agents and the sort address it).
    pub key: String,
    /// Heading (`"loc:<key>"` for a localization key). Empty uses the key.
    pub title: String,
    /// Width in UI px; `0` uses [`DEFAULT_COLUMN_PX`].
    pub width: f32,
}

impl ScoreColumn {
    /// A column keyed and titled `title`.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        let title = title.into();
        Self {
            key: title.to_lowercase(),
            title,
            width: 0.0,
        }
    }
}

/// A team.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScoreTeam {
    /// Team index, as in [`ScoreRow::team`].
    pub id: u8,
    /// Heading.
    pub name: String,
    /// Tint of the heading; `None` uses the theme's dim text.
    pub color: Option<UiColor>,
}

/// One player.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScoreRow {
    /// Stable id: breaks sort ties, and an agent addresses a row by it.
    pub id: u64,
    /// Display name.
    pub name: String,
    /// Team index, if any.
    pub team: Option<u8>,
    /// This machine's player: drawn highlighted.
    pub local: bool,
    /// One value per column (missing values show as nothing and sort first).
    pub values: Vec<ScoreValue>,
}

/// What a scoreboard shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScoreTable {
    /// Score columns.
    pub columns: Vec<ScoreColumn>,
    /// Players.
    pub rows: Vec<ScoreRow>,
    /// Teams; a row's team without an entry here gets a heading `Team <n>`.
    pub teams: Vec<ScoreTeam>,
}

/// One line the board draws, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum ScoreLine {
    /// A team heading with its totals.
    Team {
        /// Team index.
        id: u8,
        /// Heading text.
        name: String,
        /// Heading tint.
        color: Option<UiColor>,
        /// One total per column (numbers add up; text columns are empty).
        totals: Vec<String>,
    },
    /// A player.
    Player {
        /// Index in [`ScoreTable::rows`].
        row: usize,
    },
}

/// Compares two rows for the sort: by the column, then by name, then by id. The tie rule makes the
/// order one fixed answer for any input order, so two clients agree.
fn row_order(a: &ScoreRow, b: &ScoreRow, column: Option<usize>, descending: bool) -> Ordering {
    let by_column = column.map_or(Ordering::Equal, |c| match (a.values.get(c), b.values.get(c)) {
        (Some(x), Some(y)) => x.compare(y),
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    });
    let by_column = if descending {
        by_column.reverse()
    } else {
        by_column
    };
    by_column.then_with(|| a.name.cmp(&b.name)).then(a.id.cmp(&b.id))
}

/// The lines of `table`: sorted by `column`, team blocks in team order (rows without a team last)
/// when `group_by_team`.
#[must_use]
pub fn score_lines(
    table: &ScoreTable,
    column: Option<usize>,
    descending: bool,
    group_by_team: bool,
) -> Vec<ScoreLine> {
    let mut order: Vec<usize> = (0..table.rows.len()).collect();
    order.sort_by(|a, b| row_order(&table.rows[*a], &table.rows[*b], column, descending));
    if !group_by_team {
        return order.into_iter().map(|row| ScoreLine::Player { row }).collect();
    }
    let mut ids: Vec<u8> = table.rows.iter().filter_map(|r| r.team).collect();
    ids.sort_unstable();
    ids.dedup();
    let mut lines = Vec::with_capacity(order.len() + ids.len());
    for id in ids {
        let team = table.teams.iter().find(|t| t.id == id);
        lines.push(ScoreLine::Team {
            id,
            name: team.map_or_else(|| format!("Team {}", u32::from(id) + 1), |t| t.name.clone()),
            color: team.and_then(|t| t.color),
            totals: team_totals(table, id),
        });
        lines.extend(
            order
                .iter()
                .filter(|r| table.rows[**r].team == Some(id))
                .map(|row| ScoreLine::Player { row: *row }),
        );
    }
    lines.extend(
        order
            .iter()
            .filter(|r| table.rows[**r].team.is_none())
            .map(|row| ScoreLine::Player { row: *row }),
    );
    lines
}

fn team_totals(table: &ScoreTable, id: u8) -> Vec<String> {
    (0..table.columns.len())
        .map(|c| {
            let mut total = 0_i64;
            let mut any = false;
            for row in table.rows.iter().filter(|r| r.team == Some(id)) {
                if let Some(ScoreValue::Int(v)) = row.values.get(c) {
                    total = total.saturating_add(*v);
                    any = true;
                }
            }
            if any { total.to_string() } else { String::new() }
        })
        .collect()
}

/// A scoreboard. Put it on a node, or spawn it with [`spawn_scoreboard`].
#[derive(Component, Debug, Clone, PartialEq)]
pub struct Scoreboard {
    /// Heading (`"loc:<key>"` for a localization key). May be empty.
    pub title: String,
    /// The data.
    pub table: ScoreTable,
    /// The column it sorts by, by index.
    pub sort_column: Option<usize>,
    /// Largest first.
    pub descending: bool,
    /// Draw team blocks.
    pub group_by_team: bool,
    /// Whether it shows regardless of [`Self::hold_action`].
    pub visible: bool,
    /// Also show it while this input action or virtual button is held (`Scores` on Tab). Pair it
    /// with `visible: false` ([`Self::held`]).
    pub hold_action: String,
    /// Take the columns and rows from the game rules (`regia_framework::game_ui`). A game that
    /// fills [`Self::table`] itself turns this off.
    pub from_rules: bool,
}

impl Default for Scoreboard {
    fn default() -> Self {
        Self {
            title: String::new(),
            table: ScoreTable::default(),
            sort_column: Some(0),
            descending: true,
            group_by_team: true,
            visible: true,
            hold_action: String::new(),
            from_rules: true,
        }
    }
}

impl Scoreboard {
    /// A board shown only while `action` is held.
    #[must_use]
    pub fn held(action: impl Into<String>) -> Self {
        Self {
            visible: false,
            hold_action: action.into(),
            ..Self::default()
        }
    }
}

/// Runtime state of a [`Scoreboard`], kept on its node.
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct ScoreboardState {
    /// The lines drawn last frame (at most [`MAX_SCORE_ROWS`]).
    pub lines: Vec<ScoreLine>,
    /// Lines dropped by [`MAX_SCORE_ROWS`].
    pub dropped: usize,
    /// Whether the board is shown.
    pub shown: bool,
    title: Option<Entity>,
    header: Option<Entity>,
    header_cells: Vec<(Entity, Entity)>,
    body: Option<Entity>,
    rows: Vec<RowNodes>,
    column_key: Vec<(String, f32)>,
}

#[derive(Debug, Clone, PartialEq)]
struct RowNodes {
    node: Entity,
    name: Entity,
    cells: Vec<Entity>,
}

/// A heading of a board: a click sorts by `column`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScoreHeader {
    /// The board.
    pub board: Entity,
    /// The column index.
    pub column: usize,
}

#[derive(Resource, Default)]
pub(crate) struct ScoreboardEvents(MessageCursor<UiEvent>);

/// Spawns `board` on a new node under `parent`.
pub fn spawn_scoreboard(commands: &mut Commands, parent: Entity, board: Scoreboard) -> Entity {
    let node = commands
        .spawn((
            crate::node::UiNode,
            NodeName("scoreboard".to_owned()),
            NodeStyle::default(),
            board,
        ))
        .id();
    commands.entity(parent).add_child(node);
    node
}

/// Sorts the board by `column`: a new column starts largest first, the same column flips.
pub fn sort_by(board: &mut Scoreboard, column: usize) {
    if board.sort_column == Some(column) {
        board.descending = !board.descending;
    } else {
        board.sort_column = Some(column);
        board.descending = true;
    }
}

/// `Update`: a click on a heading sorts by it.
pub(crate) fn scoreboard_clicks(world: &mut World) {
    let mut cursor = world.remove_resource::<ScoreboardEvents>().unwrap_or_default();
    let clicked: Vec<Entity> = world
        .get_resource::<Messages<UiEvent>>()
        .map(|m| {
            cursor
                .0
                .read(m)
                .filter(|e| matches!(e.kind, UiEventKind::Click))
                .map(|e| e.target)
                .collect()
        })
        .unwrap_or_default();
    world.insert_resource(cursor);
    for target in clicked {
        let Some(header) = world.get::<ScoreHeader>(target).copied() else {
            continue;
        };
        if let Some(mut board) = world.get_mut::<Scoreboard>(header.board) {
            sort_by(&mut board, header.column);
        }
    }
}

fn column_px(column: &ScoreColumn) -> f32 {
    if column.width.is_finite() && column.width > 0.0 {
        column.width
    } else {
        DEFAULT_COLUMN_PX
    }
}

fn row_style() -> NodeStyle {
    NodeStyle::new(Style {
        direction: FlexDirection::Row,
        align_items: Align::Center,
        padding: Edges {
            left: Dimension::Points(10.0),
            right: Dimension::Points(10.0),
            top: Dimension::Points(3.0),
            bottom: Dimension::Points(3.0),
        },
        ..Default::default()
    })
}

fn name_cell_style() -> NodeStyle {
    NodeStyle::new(Style {
        flex_grow: 1.0,
        min_size: [Dimension::Points(140.0), Dimension::Auto],
        ..Default::default()
    })
}

fn cell_style(width: f32) -> NodeStyle {
    NodeStyle::new(Style {
        size: [Dimension::Points(width), Dimension::Auto],
        justify_content: JustifyContent::End,
        ..Default::default()
    })
}

/// (Re)builds the column-dependent parts when the columns change. Pooled rows have one cell per
/// column, so a column change drops them.
fn sync_columns(
    world: &mut World,
    entity: Entity,
    board: &Scoreboard,
    state: &mut ScoreboardState,
    colors: &ThemeColors,
) {
    let keys: Vec<(String, f32)> = board
        .table
        .columns
        .iter()
        .take(MAX_SCORE_COLUMNS)
        .map(|c| (c.key.clone(), column_px(c)))
        .collect();
    let built = state.body.is_some_and(|b| world.get_entity(b).is_ok());
    if built && keys == state.column_key {
        return;
    }
    if !built {
        if world.get::<Background>(entity).is_none() {
            world.entity_mut(entity).insert((
                Background {
                    kind: BackgroundKind::Color(colors.surface.with_alpha(0.9)),
                },
                Border {
                    color: colors.border,
                    widths: [1.0; 4],
                    radius: [6.0; 4],
                },
            ));
        }
        if let Some(mut style) = world.get_mut::<NodeStyle>(entity) {
            style.0.direction = FlexDirection::Column;
            style.0.gap = [Dimension::Points(2.0), Dimension::Points(2.0)];
            style.0.padding = Edges::px(8.0);
        }
        let title = spawn_child(world, entity, (line("", 16.0, colors.text, true), PickThrough));
        let header = spawn_child(world, entity, row_style());
        let body = spawn_child(
            world,
            entity,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                ..Default::default()
            }),
        );
        state.title = Some(title);
        state.header = Some(header);
        state.body = Some(body);
    }
    // Rows and headings are rebuilt with the new columns.
    for row in state.rows.drain(..) {
        if let Ok(e) = world.get_entity_mut(row.node) {
            e.despawn();
        }
    }
    for (cell, _) in state.header_cells.drain(..) {
        if let Ok(e) = world.get_entity_mut(cell) {
            e.despawn();
        }
    }
    if let Some(header) = state.header
        && !world.entity(header).contains::<Children>()
    {
        spawn_child(world, header, name_cell_style());
    }
    if let Some(header) = state.header {
        for (i, column) in board.table.columns.iter().take(MAX_SCORE_COLUMNS).enumerate() {
            let (button, text) = spawn_button(
                world,
                header,
                format!("scoreboard.sort.{}", column.key),
                "",
                colors,
                ScoreHeader {
                    board: entity,
                    column: i,
                },
            );
            if let Some(mut style) = world.get_mut::<NodeStyle>(button) {
                style.0.size = [Dimension::Points(column_px(column)), Dimension::Auto];
                style.0.justify_content = JustifyContent::End;
                style.0.padding = Edges::px(2.0);
            }
            state.header_cells.push((button, text));
        }
    }
    state.column_key = keys;
}

fn spawn_row(
    world: &mut World,
    body: Entity,
    board_columns: &[ScoreColumn],
    colors: &ThemeColors,
    index: usize,
) -> RowNodes {
    let node = spawn_child(
        world,
        body,
        (NodeName(format!("scoreboard.row.{index}")), row_style()),
    );
    let name = spawn_child(
        world,
        node,
        (name_cell_style(), line("", 14.0, colors.text, false), PickThrough),
    );
    let cells = board_columns
        .iter()
        .take(MAX_SCORE_COLUMNS)
        .map(|c| {
            spawn_child(
                world,
                node,
                (
                    cell_style(column_px(c)),
                    line("", 14.0, colors.text, false),
                    PickThrough,
                ),
            )
        })
        .collect();
    RowNodes { node, name, cells }
}

fn right_aligned(label: &str, px: f32, color: UiColor, bold: bool) -> crate::text::Text {
    let mut text = line(label, px, color, bold);
    text.align = TextAlign::End;
    text
}

/// Whether the hold action is down in the input actions or the virtual buttons.
fn held(world: &World, action: &str) -> bool {
    world.get_resource::<Actions>().is_some_and(|a| a.pressed(action))
        || world
            .get_resource::<VirtualButtons>()
            .is_some_and(|b| b.0.get(action).copied().unwrap_or(false))
}

/// `PostUpdate`, before layout: shows, hides and refreshes each board.
pub(crate) fn scoreboard_update(world: &mut World) {
    let boards: Vec<Entity> = world
        .query_filtered::<Entity, With<Scoreboard>>()
        .iter(world)
        .collect();
    if boards.is_empty() {
        return;
    }
    let _span = tracing::trace_span!("ui_scoreboard").entered();
    let colors = theme_colors(world);
    for entity in boards {
        let Some(board) = world.get::<Scoreboard>(entity).cloned() else {
            continue;
        };
        let mut state: ScoreboardState = take_state(world, entity);
        sync_columns(world, entity, &board, &mut state, &colors);
        let shown = board.visible || (!board.hold_action.is_empty() && held(world, &board.hold_action));
        state.shown = shown;
        set_shown(world, entity, shown);
        if shown {
            refresh(world, &board, &mut state, &colors);
        }
        put_state(world, entity, state);
    }
}

fn refresh(world: &mut World, board: &Scoreboard, state: &mut ScoreboardState, colors: &ThemeColors) {
    if let Some(title) = state.title {
        set_if_changed(world, title, line(&board.title, 16.0, colors.text, true));
        set_shown(world, title, !board.title.is_empty());
    }
    // The headings, with the sort arrow on the sorted one.
    for (i, column) in board.table.columns.iter().take(MAX_SCORE_COLUMNS).enumerate() {
        let Some((_, text)) = state.header_cells.get(i).copied() else {
            continue;
        };
        let title = if column.title.is_empty() {
            &column.key
        } else {
            &column.title
        };
        let label = match (board.sort_column == Some(i), board.descending) {
            (true, true) => format!("{title} \u{25BC}"),
            (true, false) => format!("{title} \u{25B2}"),
            _ => title.clone(),
        };
        set_if_changed(world, text, right_aligned(&label, 12.0, colors.text_dim, true));
    }
    let sort = board.sort_column.filter(|c| *c < board.table.columns.len());
    let mut lines = score_lines(&board.table, sort, board.descending, board.group_by_team);
    state.dropped = lines.len().saturating_sub(MAX_SCORE_ROWS);
    lines.truncate(MAX_SCORE_ROWS);
    let Some(body) = state.body else { return };
    while state.rows.len() < lines.len() {
        let index = state.rows.len();
        let row = spawn_row(world, body, &board.table.columns, colors, index);
        state.rows.push(row);
    }
    for (i, drawn) in lines.iter().enumerate() {
        let Some(nodes) = state.rows.get(i).cloned() else {
            continue;
        };
        set_shown(world, nodes.node, true);
        match drawn {
            ScoreLine::Team {
                name, color, totals, ..
            } => {
                let tint = color.unwrap_or(colors.text_dim);
                set_if_changed(world, nodes.name, line(name, 13.0, tint, true));
                for (c, cell) in nodes.cells.iter().enumerate() {
                    let total = totals.get(c).map_or("", String::as_str);
                    set_if_changed(world, *cell, right_aligned(total, 13.0, tint, true));
                }
                set_if_changed(
                    world,
                    nodes.node,
                    Background {
                        kind: BackgroundKind::Color(tint.with_alpha(0.14)),
                    },
                );
            }
            ScoreLine::Player { row } => {
                let Some(player) = board.table.rows.get(*row) else {
                    continue;
                };
                let tone = if player.local { colors.primary } else { colors.text };
                // A player's own words: never a localization key.
                set_if_changed(
                    world,
                    nodes.name,
                    literal_line(&player.name, 14.0, tone, player.local),
                );
                for (c, cell) in nodes.cells.iter().enumerate() {
                    let value = player.values.get(c).map(ScoreValue::display).unwrap_or_default();
                    let mut text = literal_line(&value, 14.0, tone, player.local);
                    text.align = TextAlign::End;
                    set_if_changed(world, *cell, text);
                }
                let fill = if player.local {
                    colors.primary.with_alpha(0.16)
                } else {
                    UiColor::TRANSPARENT
                };
                set_if_changed(
                    world,
                    nodes.node,
                    Background {
                        kind: BackgroundKind::Color(fill),
                    },
                );
            }
        }
    }
    for extra in state.rows.iter().skip(lines.len()) {
        set_shown(world, extra.node, false);
    }
    state.lines = lines;
}

/// Invented rows for the designer's preview, on boards with none.
pub(crate) fn fill_preview(world: &mut World) {
    let boards: Vec<Entity> = world
        .query_filtered::<Entity, With<Scoreboard>>()
        .iter(world)
        .collect();
    for entity in boards {
        let Some(mut board) = world.get_mut::<Scoreboard>(entity) else {
            continue;
        };
        if !board.table.rows.is_empty() {
            continue;
        }
        if board.table.columns.is_empty() {
            board.table.columns = vec![ScoreColumn::new("Score"), ScoreColumn::new("Deaths")];
        }
        let columns = board.table.columns.len();
        board.table.rows = ["Alpha", "Bravo", "Charlie", "Delta"]
            .iter()
            .enumerate()
            .map(|(i, name)| ScoreRow {
                id: i as u64 + 1,
                name: (*name).to_owned(),
                team: Some((i % 2) as u8),
                local: i == 1,
                values: (0..columns)
                    .map(|c| ScoreValue::Int(((4 - i) * (c + 2)) as i64))
                    .collect(),
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> ScoreTable {
        ScoreTable {
            columns: vec![ScoreColumn::new("Tags"), ScoreColumn::new("Tagged")],
            rows: vec![
                ScoreRow {
                    id: 1,
                    name: "Ann".into(),
                    team: Some(1),
                    local: false,
                    values: vec![ScoreValue::Int(2), ScoreValue::Int(5)],
                },
                ScoreRow {
                    id: 2,
                    name: "Bob".into(),
                    team: Some(0),
                    local: true,
                    values: vec![ScoreValue::Int(7), ScoreValue::Int(1)],
                },
                ScoreRow {
                    id: 3,
                    name: "Cat".into(),
                    team: Some(1),
                    local: false,
                    values: vec![ScoreValue::Int(7), ScoreValue::Int(3)],
                },
                ScoreRow {
                    id: 4,
                    name: "Dee".into(),
                    team: None,
                    local: false,
                    values: vec![ScoreValue::Text("spectator".into())],
                },
            ],
            teams: vec![ScoreTeam {
                id: 0,
                name: "Red".into(),
                color: None,
            }],
        }
    }

    fn names(table: &ScoreTable, lines: &[ScoreLine]) -> Vec<String> {
        lines
            .iter()
            .map(|l| match l {
                ScoreLine::Team { name, .. } => format!("[{name}]"),
                ScoreLine::Player { row } => table.rows[*row].name.clone(),
            })
            .collect()
    }

    #[test]
    fn descending_sort_breaks_ties_by_name_then_id() {
        let t = table();
        let lines = score_lines(&t, Some(0), true, false);
        // Text sorts above numbers when descending, then 7 (Bob, Cat by name), then 2.
        assert_eq!(names(&t, &lines), ["Dee", "Bob", "Cat", "Ann"]);
        let up = score_lines(&t, Some(0), false, false);
        assert_eq!(names(&t, &up), ["Ann", "Bob", "Cat", "Dee"]);
    }

    #[test]
    fn the_order_does_not_depend_on_the_input_order() {
        let t = table();
        let mut shuffled = t.clone();
        shuffled.rows.reverse();
        assert_eq!(
            names(&t, &score_lines(&t, Some(0), true, true)),
            names(&shuffled, &score_lines(&shuffled, Some(0), true, true)),
        );
    }

    #[test]
    fn teams_group_with_totals_and_teamless_rows_come_last() {
        let t = table();
        let lines = score_lines(&t, Some(0), true, true);
        assert_eq!(
            names(&t, &lines),
            ["[Red]", "Bob", "[Team 2]", "Cat", "Ann", "Dee"]
        );
        let ScoreLine::Team { totals, .. } = &lines[2] else {
            panic!("{lines:?}");
        };
        assert_eq!(totals, &["9".to_owned(), "8".to_owned()]);
    }

    #[test]
    fn a_text_or_missing_column_totals_to_nothing_and_a_huge_sum_saturates() {
        let mut t = table();
        t.columns.push(ScoreColumn::new("Class"));
        t.rows[0].values.push(ScoreValue::Text("medic".into()));
        assert_eq!(team_totals(&t, 1)[2], "");
        t.rows[0].values[0] = ScoreValue::Int(i64::MAX);
        t.rows[2].values[0] = ScoreValue::Int(i64::MAX);
        assert_eq!(team_totals(&t, 1)[0], i64::MAX.to_string());
    }

    #[test]
    fn sorting_a_column_again_flips_it() {
        let mut board = Scoreboard::default();
        assert_eq!((board.sort_column, board.descending), (Some(0), true));
        sort_by(&mut board, 0);
        assert_eq!((board.sort_column, board.descending), (Some(0), false));
        sort_by(&mut board, 1);
        assert_eq!((board.sort_column, board.descending), (Some(1), true));
    }

    #[test]
    fn no_sort_column_orders_by_name() {
        let t = table();
        let lines = score_lines(&t, None, true, false);
        assert_eq!(names(&t, &lines), ["Ann", "Bob", "Cat", "Dee"]);
    }
}
