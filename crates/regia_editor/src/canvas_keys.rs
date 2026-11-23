// SPDX-License-Identifier: MPL-2.0
//! Keyboard routes and named list views for the editor's painted canvases (ADR-7383,
//! `docs/evidence/editor-a11y-2026-11.md` FU-5).
//!
//! A canvas (a node graph, a curve, a section strip, a flame view or a trace timeline) paints its
//! items, so a screen reader finds no widget for them and every edit needs the mouse. Each canvas
//! panel draws a list beside it: one focusable row per item, named by what it shows. The rows are
//! laid out only while in view (`ScrollArea::show_rows`), so a 5 000-node graph costs the rows on
//! screen. With a row focused:
//!
//! | Key | Does |
//! |---|---|
//! | Up / Down | Moves the focus to the previous / next row (scrolling it into view) |
//! | Enter or Space | Activates the row (a graph selects the node and frames it on the canvas) |
//! | Delete | Removes the item |
//! | Alt+arrow | Graphs: moves the node by [`GRID_STEP`] (one undo entry per press); Alt+Shift+arrow by [`COARSE_STEP`] |
//! | Left / Right | Sequencer: moves the key or section by one frame (one undo entry per press); with Shift the panel may take a coarser step |
//! | Ctrl+Shift+A | Graphs: opens the node palette; the chosen node is added wired after the row's node |
//! | Ctrl+Shift+C | Graphs with wires: starts a connection from the row (opens **Connect…**); Enter in its last field finishes it, Escape cancels |
//! | Arrows, then Enter | 2D tab: the tile palette moves its focus (Up/Down a row) and picks; the map cursor moves a cell and paints it (Delete erases), `mode2d::panel` (ADR-7742) |
//!
//! The graph list also has **Add node…**, **Connect…** and **Delete** buttons acting on the last
//! focused row, so Shift+Tab from a row reaches them. **Connect…** wires an output pin of that node
//! to an input pin of another node, both typed by name. The panels turn each [`GraphAction`] into
//! the same edit their agent methods make (`editor.matgraph_edit`, `procgen.graph_edit`,
//! `editor.seq_curve_edit`, `editor.seq_section_edit`).
//!
//! [`name_canvas`] gives the painted region itself an AccessKit role ([`Role::Canvas`]) and a name
//! such as "Material graph canvas, 12 nodes".

use std::hash::Hash;

use egui::accesskit::Role;
use egui_snarl::{NodeId, Snarl};

/// How far Alt+arrow moves a graph node, in canvas units.
pub const GRID_STEP: f32 = 16.0;

/// How far Alt+Shift+arrow moves a graph node, in canvas units (four grid steps).
pub const COARSE_STEP: f32 = GRID_STEP * 4.0;

/// Height of a list view when the panel has more room, points.
pub const LIST_HEIGHT: f32 = 260.0;

/// The focus and view of one list (panel state, not undone).
#[derive(Clone, Debug, Default)]
pub struct ListKeys {
    /// The row that last had the keyboard focus (or was activated); the toolbar buttons act on it.
    pub current: Option<usize>,
    /// A row whose turn it is to take the focus once it is laid out.
    want_focus: Option<usize>,
    /// A scroll offset to apply on the next draw (bring `want_focus` into view).
    scroll_to: Option<f32>,
    /// Whether a row of this list had the keyboard focus in the last frame.
    pub row_focused: bool,
    /// Rows laid out in the last frame.
    pub drawn: usize,
    /// Rows in the list in the last frame.
    pub total: usize,
    /// The rows in view in the last frame.
    shown: std::ops::Range<usize>,
}

impl ListKeys {
    /// The row that has the keyboard focus now (`None` when no row of this list has it). A canvas
    /// draws its focus ring ([`focus_ring`]) around this row's item.
    #[must_use]
    pub fn focused(&self) -> Option<usize> {
        self.current.filter(|_| self.row_focused)
    }

    /// Gives the keyboard focus to `row` on the next draw (scrolling it into view).
    pub fn focus(&mut self, row: usize) {
        self.want_focus = Some(row);
        self.current = Some(row);
    }

    /// The item the focus belongs to is now at `row` (rows moved: an item was added or removed
    /// before it). A focused list moves the keyboard focus along.
    pub fn retarget(&mut self, row: Option<usize>) {
        if row == self.current {
            return;
        }
        self.current = row;
        if self.row_focused || self.want_focus.is_some() {
            self.want_focus = row;
        }
    }
}

/// A key a focused row reports to its panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKey {
    /// Enter, Space or a click.
    Activate(usize),
    /// Delete.
    Delete(usize),
    /// Ctrl+Shift+A.
    Palette(usize),
    /// Ctrl+Shift+C: start a connection from the row.
    Connect(usize),
    /// An arrow: `dx`, `dy` in -1..=1 (screen directions, down is +1), `alt` with Alt held,
    /// `coarse` with Shift held. Up and Down without Alt move the focus and are not reported.
    Arrow {
        /// The focused row.
        row: usize,
        /// -1 left, 1 right.
        dx: i32,
        /// -1 up, 1 down.
        dy: i32,
        /// Alt was held.
        alt: bool,
        /// Shift was held (a coarse step).
        coarse: bool,
    },
}

/// Draws `len` focusable rows named by `name(i)` in a scroll area at most `height` points tall,
/// laying out only the rows in view. `selected` is highlighted. Returns what the keyboard (or a
/// click) did to them.
pub fn list_rows(
    ui: &mut egui::Ui,
    id_salt: impl Hash + std::fmt::Debug,
    keys: &mut ListKeys,
    len: usize,
    height: f32,
    selected: Option<usize>,
    name: &dyn Fn(usize) -> String,
) -> Vec<RowKey> {
    let mut events = Vec::new();
    keys.total = len;
    keys.drawn = 0;
    if keys.current.is_some_and(|c| c >= len) {
        keys.current = len.checked_sub(1);
    }
    if keys.want_focus.is_some_and(|w| w >= len) {
        keys.want_focus = None;
    }
    let focused_at_start = ui.memory(|m| m.focused());
    let row_height = ui.spacing().interact_size.y;
    let pitch = row_height + ui.spacing().item_spacing.y;
    let salt = egui::Id::new(id_salt);
    let mut area = egui::ScrollArea::vertical()
        .id_salt(salt)
        .max_height(height)
        .auto_shrink([false, true]);
    if let Some(want) = keys.want_focus
        && !keys.shown.contains(&want)
    {
        // Out of view: scroll so the row is laid out (top third of the list) this frame.
        keys.scroll_to = Some((want as f32 * pitch - height / 3.0).max(0.0));
    }
    if let Some(offset) = keys.scroll_to.take() {
        area = area.vertical_scroll_offset(offset);
    }
    let mut focused_now = false;
    ui.push_id(salt, |ui| {
        area.show_rows(ui, row_height, len, |ui, range| {
            keys.shown = range.clone();
            for i in range {
                keys.drawn += 1;
                // An explicit id per row: an id derived from the parent's would change with
                // what was drawn before the list (egui mixes in an auto counter), and egui drops
                // the focus of a widget whose id disappears.
                let row = egui::UiBuilder::new().id(salt.with(i));
                let response = ui
                    .scope_builder(row, |ui| ui.selectable_label(selected == Some(i), name(i)))
                    .inner;
                if response.clicked() {
                    keys.current = Some(i);
                    events.push(RowKey::Activate(i));
                }
                if keys.want_focus == Some(i) {
                    response.request_focus();
                    response.scroll_to_me(None);
                    keys.want_focus = None;
                }
                if !response.has_focus() {
                    continue;
                }
                focused_now = true;
                keys.current = Some(i);
                ui.memory_mut(|m| {
                    m.set_focus_lock_filter(
                        response.id,
                        egui::EventFilter {
                            horizontal_arrows: true,
                            vertical_arrows: true,
                            ..Default::default()
                        },
                    );
                });
                // Keys belong to the row that had the focus when the frame began: a row that just
                // took it (Down from the row above) must not see the same press again.
                if focused_at_start != Some(response.id) {
                    continue;
                }
                row_keys(ui, i, len, keys, &mut events);
            }
        })
    });
    keys.row_focused = focused_now;
    events
}

/// The keys of focused row `i`.
fn row_keys(ui: &egui::Ui, i: usize, len: usize, keys: &mut ListKeys, events: &mut Vec<RowKey>) {
    use egui::{Key, Modifiers};
    let shortcut = Modifiers::COMMAND | Modifiers::SHIFT;
    let mut handled = false;
    ui.input_mut(|input| {
        if input.consume_key(shortcut, Key::A) {
            events.push(RowKey::Palette(i));
        }
        if input.consume_key(shortcut, Key::C) {
            events.push(RowKey::Connect(i));
        }
        if input.consume_key(Modifiers::NONE, Key::Delete) {
            events.push(RowKey::Delete(i));
        }
        for (key, dx, dy) in [
            (Key::ArrowLeft, -1, 0),
            (Key::ArrowRight, 1, 0),
            (Key::ArrowUp, 0, -1),
            (Key::ArrowDown, 0, 1),
        ] {
            let alt_coarse = input.consume_key(Modifiers::ALT | Modifiers::SHIFT, key);
            let alt = alt_coarse || input.consume_key(Modifiers::ALT, key);
            // Shift+Left / Shift+Right: a coarse horizontal step (Shift+Up / Down move the focus
            // like Up / Down: egui matches Shift loosely).
            let plain_coarse = !alt && dy == 0 && input.consume_key(Modifiers::SHIFT, key);
            let plain = !alt && (plain_coarse || input.consume_key(Modifiers::NONE, key));
            if alt || plain {
                handled = true;
            }
            if alt {
                events.push(RowKey::Arrow {
                    row: i,
                    dx,
                    dy,
                    alt: true,
                    coarse: alt_coarse,
                });
            } else if plain {
                match dy {
                    -1 if i > 0 => keys.focus(i - 1),
                    1 if i + 1 < len => keys.focus(i + 1),
                    0 => events.push(RowKey::Arrow {
                        row: i,
                        dx,
                        dy,
                        alt: false,
                        coarse: plain_coarse,
                    }),
                    _ => {}
                }
            }
        }
    });
    if handled {
        // egui moves the focus with the arrows too (the focus-lock filter only holds for a widget
        // that had the focus last frame): the list moved it already.
        ui.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
    }
}

/// Whether a panel write keyed `id` may commit in egui frame `frame` (it then stamps the frame in
/// egui's temp memory). egui runs a panel's UI twice in one frame when a pass is discarded (a
/// layout change), and the second pass sees the same click: a toggle written on both passes flips
/// back. Commit toggles, queued triggers and one-shot requests through this gate (review
/// sprint-w12-rev-g7 #4, #5, #9, #10).
pub fn first_in_frame(ctx: &egui::Context, id: egui::Id, frame: u64) -> bool {
    let id = id.with("regia-first-in-frame");
    if ctx.data(|d| d.get_temp::<u64>(id)) == Some(frame) {
        return false;
    }
    ctx.data_mut(|d| d.insert_temp(id, frame));
    true
}

/// [`first_in_frame`] for the frame `ui` draws.
pub fn first_in_this_frame(ui: &egui::Ui, id: egui::Id) -> bool {
    first_in_frame(ui.ctx(), id, ui.ctx().cumulative_frame_nr())
}

/// Gives the painted region `id` (the response of the canvas widget) the AccessKit role
/// [`Role::Canvas`] and the name `label`. Nothing happens when AccessKit is off.
pub fn name_canvas(ctx: &egui::Context, id: egui::Id, label: &str) {
    ctx.accesskit_node_builder(id, |node| {
        node.set_role(Role::Canvas);
        node.set_label(label.to_string());
    });
}

/// The keyboard focus color of the theme: the accent of a selected or focused item
/// (`AccentTokens::select`).
#[must_use]
pub fn focus_color(ctx: &egui::Context) -> egui::Color32 {
    crate::theme::ui_tokens(ctx).accents.select
}

/// Width of a canvas focus ring, points.
pub const FOCUS_RING_WIDTH: f32 = 2.0;

/// Draws the keyboard focus ring around a painted item's `rect`: an outline in [`focus_color`]
/// a little outside the item, so it shows next to the item's own selection outline.
pub fn focus_ring(painter: &egui::Painter, rect: egui::Rect) {
    let color = focus_color(painter.ctx());
    painter.rect_stroke(
        rect.expand(3.0),
        egui::CornerRadius::same(3),
        egui::Stroke::new(FOCUS_RING_WIDTH, color),
        egui::StrokeKind::Outside,
    );
}

/// The fields of the **Connect…** form, typed by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectForm {
    /// The output pin of the focused node (empty: its first output).
    pub output: String,
    /// The node to wire into, by its row name or node name.
    pub target: String,
    /// The input pin of `target`, by name.
    pub input: String,
}

/// What a graph's keyboard route asks its panel to do.
#[derive(Clone, Debug, PartialEq)]
pub enum GraphAction {
    /// Select node `row` and frame it on the canvas (Enter).
    Activate(usize),
    /// Remove node `row`.
    Delete(usize),
    /// Move node `row` by `delta` canvas units (one undo entry).
    Nudge(usize, [f32; 2]),
    /// Add a node of `kind` wired after node `row` (one undo entry).
    AddAfter(usize, String),
    /// Wire an output of node `row` to the input the form names.
    Connect(usize, ConnectForm),
}

/// The keyboard state of a graph panel: its node list, the palette and the connect form.
#[derive(Clone, Debug, Default)]
pub struct GraphKeys {
    /// The node list.
    pub list: ListKeys,
    /// The palette filter while the palette is open.
    pub palette: Option<String>,
    /// The connect form while it is open.
    pub connect: Option<ConnectForm>,
    /// Bumped by each framing request: the canvas id carries it, so the canvas fits its view
    /// anew (egui-snarl 0.12 has no public view transform; see [`frame_snarl`]).
    pub frame_epoch: u64,
    /// The node to frame on this draw.
    pub frame: Option<NodeId>,
    /// Focus the palette filter (or the first connect field) on the next draw.
    focus_form: bool,
}

/// What a graph panel tells [`graph_keys_ui`] about its nodes.
pub struct GraphRows<'a> {
    /// Number of nodes.
    pub len: usize,
    /// The row (and accessible) name of node `i`: its title.
    pub name: &'a dyn Fn(usize) -> String,
    /// The selected node's row.
    pub selected: Option<usize>,
    /// The palette entries for node `i` (kinds that can be added after it), filtered by text.
    pub kinds: &'a dyn Fn(usize, &str) -> Vec<String>,
    /// The graph's nodes have named outputs (the connect form asks for the output pin).
    pub named_outputs: bool,
    /// Edits are refused (an instance's parent graph): only the list and Enter work.
    pub read_only: bool,
    /// The connect form asks for the target's input pin (graphs); without, Enter in the target
    /// field connects (a state machine's transition has no pins).
    pub input_pins: bool,
    /// Delete removes a row's item (a canvas whose items are not removed from the canvas says
    /// `false`: the key and button do nothing).
    pub deletable: bool,
    /// The list heading ("Nodes").
    pub heading: &'a str,
    /// What a row is, for the toolbar buttons ("node": **Add node…**, **Delete node**).
    pub noun: &'a str,
}

/// Draws the "Nodes" list of a graph with its toolbar, palette and connect form; returns what the
/// keyboard asked for. `what` names the graph in the heading ("Material graph").
pub fn graph_keys_ui(
    ui: &mut egui::Ui,
    id_salt: impl Hash + Copy + std::fmt::Debug,
    keys: &mut GraphKeys,
    rows: &GraphRows<'_>,
) -> Vec<GraphAction> {
    let mut out = Vec::new();
    ui.strong(format!("{} ({})", rows.heading, rows.len));
    let current = keys.list.current.filter(|c| *c < rows.len);
    let editable = current.is_some() && !rows.read_only;
    // The palette has nothing to offer for a canvas without kinds to add.
    let addable = editable && current.is_some_and(|row| !(rows.kinds)(row, "").is_empty());
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(addable, egui::Button::new(format!("Add {}…", rows.noun)))
            .on_hover_text(format!(
                "Add a {} wired after the focused {} (Ctrl+Shift+A)",
                rows.noun, rows.noun
            ))
            .clicked()
        {
            keys.palette = Some(String::new());
            keys.connect = None;
            keys.focus_form = true;
        }
        if ui
            .add_enabled(editable, egui::Button::new("Connect…"))
            .on_hover_text(format!(
                "Wire the focused {} to another {}, by name (Ctrl+Shift+C)",
                rows.noun, rows.noun
            ))
            .clicked()
        {
            keys.connect = Some(ConnectForm::default());
            keys.palette = None;
            keys.focus_form = true;
        }
        if ui
            .add_enabled(
                editable && rows.deletable,
                egui::Button::new(format!("Delete {}", rows.noun)),
            )
            .on_hover_text(format!("Remove the focused {} (Delete)", rows.noun))
            .clicked()
            && let Some(row) = current
        {
            out.push(GraphAction::Delete(row));
        }
    });
    if let (Some(row), Some(filter)) = (current, keys.palette.as_mut()) {
        let cancelled = palette_ui(ui, row, filter, &mut keys.focus_form, rows, &mut out);
        if cancelled || out.iter().any(|a| matches!(a, GraphAction::AddAfter(..))) {
            keys.palette = None;
            keys.list.focus(row);
        }
    }
    if let (Some(row), Some(form)) = (current, keys.connect.as_mut()) {
        let (submit, cancel) = connect_ui(ui, form, &mut keys.focus_form, rows);
        if submit {
            out.push(GraphAction::Connect(row, form.clone()));
        }
        if submit || cancel {
            keys.connect = None;
            keys.list.focus(row);
        }
    }
    let height = ui
        .available_height()
        .clamp(80.0, LIST_HEIGHT.max(ui.available_height() - 8.0));
    for key in list_rows(
        ui,
        (id_salt, "rows"),
        &mut keys.list,
        rows.len,
        height,
        rows.selected,
        rows.name,
    ) {
        match key {
            RowKey::Activate(row) => out.push(GraphAction::Activate(row)),
            RowKey::Delete(row) if !rows.read_only && rows.deletable => out.push(GraphAction::Delete(row)),
            RowKey::Palette(row) if !rows.read_only && !(rows.kinds)(row, "").is_empty() => {
                keys.palette = Some(String::new());
                keys.connect = None;
                keys.focus_form = true;
            }
            RowKey::Connect(_) if !rows.read_only => {
                keys.connect = Some(ConnectForm::default());
                keys.palette = None;
                keys.focus_form = true;
            }
            RowKey::Arrow {
                row,
                dx,
                dy,
                alt: true,
                coarse,
            } if !rows.read_only => {
                let step = if coarse { COARSE_STEP } else { GRID_STEP };
                out.push(GraphAction::Nudge(row, [dx as f32 * step, dy as f32 * step]));
            }
            _ => {}
        }
    }
    out
}

/// The palette: a filter field (Enter adds the first match, Escape closes) and the matching kinds
/// as buttons. Returns whether Escape closed it.
fn palette_ui(
    ui: &mut egui::Ui,
    row: usize,
    filter: &mut String,
    focus_form: &mut bool,
    rows: &GraphRows<'_>,
    out: &mut Vec<GraphAction>,
) -> bool {
    // Escape closes the palette (rev-claude-53-k finding 2), as it closes the connect form.
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        return true;
    }
    let label = ui.label(format!("Add after {}:", (rows.name)(row)));
    let field = ui
        .add(egui::TextEdit::singleline(filter).hint_text("filter node kinds, Enter adds the first"))
        .labelled_by(label.id);
    if std::mem::take(focus_form) {
        field.request_focus();
    }
    let kinds = (rows.kinds)(row, filter);
    if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        if let Some(first) = kinds.first() {
            out.push(GraphAction::AddAfter(row, first.clone()));
        }
        return false;
    }
    egui::ScrollArea::vertical()
        .id_salt("canvas-keys-palette")
        .max_height(160.0)
        .show(ui, |ui| {
            for kind in kinds {
                if ui.button(&kind).clicked() {
                    out.push(GraphAction::AddAfter(row, kind));
                }
            }
        });
    false
}

/// The connect form; returns (submitted, cancelled).
fn connect_ui(
    ui: &mut egui::Ui,
    form: &mut ConnectForm,
    focus_form: &mut bool,
    rows: &GraphRows<'_>,
) -> (bool, bool) {
    let mut submit = false;
    let mut first = true;
    let mut field = |ui: &mut egui::Ui, text: &str, value: &mut String, hint: &str| {
        ui.horizontal(|ui| {
            let label = ui.label(text);
            let response = ui
                .add(
                    egui::TextEdit::singleline(value)
                        .hint_text(hint)
                        .desired_width(140.0),
                )
                .labelled_by(label.id);
            if first && std::mem::take(focus_form) {
                response.request_focus();
            }
            first = false;
            response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
        })
        .inner
    };
    if rows.named_outputs {
        field(ui, "From output", &mut form.output, "first output");
    }
    let to = format!("To {}", rows.noun);
    if rows.input_pins {
        field(ui, &to, &mut form.target, "name");
        submit |= field(ui, "Input pin", &mut form.input, "pin name, Enter connects");
    } else {
        submit |= field(ui, &to, &mut form.target, "name, Enter connects");
    }
    let mut cancel = false;
    ui.horizontal(|ui| {
        submit |= ui.button("Connect").clicked();
        cancel = ui.button("Cancel").clicked();
    });
    cancel |= ui.input(|i| i.key_pressed(egui::Key::Escape));
    (submit, cancel)
}

/// A copy of `snarl` with only `node` in it, at its position. Drawn once under a fresh canvas id
/// (see [`GraphKeys::frame_epoch`]), egui-snarl's first-frame fit centers the view on that node;
/// the full graph then draws under the same id and keeps that view.
#[must_use]
pub fn frame_snarl<T: Clone>(snarl: &Snarl<T>, node: NodeId) -> Snarl<T> {
    let mut one = Snarl::new();
    if let Some(info) = snarl.get_node_info(node) {
        one.insert_node(info.pos, info.value.clone());
    }
    one
}

#[cfg(test)]
pub(crate) mod test_input {
    //! Synthetic keyboard input for headless panel tests (no pointer events).

    use egui::accesskit::TreeUpdate;

    /// A raw input of `size` carrying `events`.
    #[must_use]
    pub fn raw(size: [f32; 2], events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size[0], size[1]),
            )),
            events,
            ..Default::default()
        }
    }

    /// A key press with `modifiers`.
    #[must_use]
    pub fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// Typed text.
    #[must_use]
    pub fn text(s: &str) -> egui::Event {
        egui::Event::Text(s.to_string())
    }

    /// The accessible name of the focused node of `update` (`None`: nothing focused).
    #[must_use]
    pub fn focused_name(update: &TreeUpdate) -> Option<String> {
        update
            .nodes
            .iter()
            .find(|(id, _)| *id == update.focus)
            .and_then(|(_, node)| node.label().map(str::to_string))
    }

    /// Runs one frame of `draw` with `events` (AccessKit on) and returns its tree update.
    pub fn frame(
        ctx: &egui::Context,
        size: [f32; 2],
        events: Vec<egui::Event>,
        mut draw: impl FnMut(&mut egui::Ui),
    ) -> TreeUpdate {
        ctx.enable_accesskit();
        let mut out = ctx.run_ui(raw(size, events), |ui| draw(ui));
        out.textures_delta.clear();
        out.platform_output
            .accesskit_update
            .unwrap_or_else(|| panic!("accesskit is on"))
    }

    /// Presses Tab (Shift+Tab with `back`) until the focused widget is named `name`; panics after
    /// `limit` presses. Returns the tree update of the last frame.
    pub fn tab_to(
        ctx: &egui::Context,
        size: [f32; 2],
        name: &str,
        back: bool,
        limit: usize,
        mut draw: impl FnMut(&mut egui::Ui),
    ) -> TreeUpdate {
        let modifiers = if back {
            egui::Modifiers::SHIFT
        } else {
            egui::Modifiers::NONE
        };
        let mut seen = Vec::new();
        for _ in 0..limit {
            // Shift+Tab moves the focus at the start of the next frame: settle before reading it.
            frame(ctx, size, vec![key(egui::Key::Tab, modifiers)], &mut draw);
            let update = frame(ctx, size, vec![], &mut draw);
            let focused = focused_name(&update);
            if focused.as_deref() == Some(name) {
                return update;
            }
            seen.push(focused.unwrap_or_default());
        }
        panic!("Tab never reached `{name}`; focus went through {seen:?}");
    }

    /// The labels of every node of `update` with role `role`.
    #[must_use]
    pub fn labels_of(update: &TreeUpdate, role: egui::accesskit::Role) -> Vec<String> {
        update
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == role)
            .filter_map(|(_, node)| node.label().map(str::to_string))
            .collect()
    }
}

/// A key routed to the separate play window's painted virtual pad (ADR-8205).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayDeviceKey {
    /// Move to the next button (or the previous one with Shift).
    Traverse,
    /// Hold the focused button until key release.
    Activate,
    /// Clear pad focus and release its held button.
    Leave,
}

/// Canvas keyboard entry for the play-device overlay. The window applies this only
/// while gameplay has not captured input; Space/Enter/Escape need a focused button.
#[must_use]
pub fn play_device_key(physical: &str, focused: bool) -> Option<PlayDeviceKey> {
    match physical {
        "Tab" => Some(PlayDeviceKey::Traverse),
        "Space" | "Enter" if focused => Some(PlayDeviceKey::Activate),
        "Escape" if focused => Some(PlayDeviceKey::Leave),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::test_input::{key, raw};
    use super::*;

    #[test]
    fn a_panel_write_commits_once_per_egui_frame_and_key() {
        let ctx = egui::Context::default();
        let (a, b) = (egui::Id::new("a"), egui::Id::new("b"));
        assert!(first_in_frame(&ctx, a, 3));
        assert!(
            !first_in_frame(&ctx, a, 3),
            "the discarded second pass of frame 3"
        );
        assert!(first_in_frame(&ctx, b, 3), "another key has its own stamp");
        assert!(first_in_frame(&ctx, a, 4), "the next frame");
    }
    use egui::{Key, Modifiers};

    fn frame(ctx: &egui::Context, keys: &mut ListKeys, len: usize, events: Vec<egui::Event>) -> Vec<RowKey> {
        let mut got = Vec::new();
        let mut out = ctx.run_ui(raw([400.0, 300.0], events), |ui| {
            got = list_rows(ui, "t", keys, len, 200.0, None, &|i| format!("row {i}"));
        });
        out.textures_delta.clear();
        got
    }

    #[test]
    fn tab_down_and_keys_reach_the_rows_without_a_pointer() {
        let ctx = egui::Context::default();
        let mut keys = ListKeys::default();
        frame(&ctx, &mut keys, 10, vec![]);
        frame(&ctx, &mut keys, 10, vec![key(Key::Tab, Modifiers::NONE)]);
        assert_eq!(keys.current, Some(0), "Tab focuses the first row");
        assert!(keys.row_focused);
        frame(&ctx, &mut keys, 10, vec![key(Key::ArrowDown, Modifiers::NONE)]);
        frame(&ctx, &mut keys, 10, vec![]);
        assert_eq!(keys.current, Some(1), "Down moves one row, not two");
        let got = frame(&ctx, &mut keys, 10, vec![key(Key::Enter, Modifiers::NONE)]);
        assert_eq!(got, [RowKey::Activate(1)]);
        let got = frame(&ctx, &mut keys, 10, vec![key(Key::Delete, Modifiers::NONE)]);
        assert_eq!(got, [RowKey::Delete(1)]);
        let got = frame(&ctx, &mut keys, 10, vec![key(Key::ArrowRight, Modifiers::ALT)]);
        assert_eq!(
            got,
            [RowKey::Arrow {
                row: 1,
                dx: 1,
                dy: 0,
                alt: true,
                coarse: false
            }]
        );
        let got = frame(
            &ctx,
            &mut keys,
            10,
            vec![key(Key::ArrowDown, Modifiers::ALT | Modifiers::SHIFT)],
        );
        assert_eq!(
            got,
            [RowKey::Arrow {
                row: 1,
                dx: 0,
                dy: 1,
                alt: true,
                coarse: true
            }],
            "Alt+Shift+arrow is a coarse nudge"
        );
        let got = frame(&ctx, &mut keys, 10, vec![key(Key::ArrowLeft, Modifiers::SHIFT)]);
        assert_eq!(
            got,
            [RowKey::Arrow {
                row: 1,
                dx: -1,
                dy: 0,
                alt: false,
                coarse: true
            }],
            "Shift+Left is a coarse step"
        );
        assert_eq!(keys.focused(), Some(1), "the focused row");
        let got = frame(
            &ctx,
            &mut keys,
            10,
            vec![key(Key::A, Modifiers::COMMAND | Modifiers::SHIFT)],
        );
        assert_eq!(got, [RowKey::Palette(1)]);
        let got = frame(
            &ctx,
            &mut keys,
            10,
            vec![key(Key::C, Modifiers::COMMAND | Modifiers::SHIFT)],
        );
        assert_eq!(got, [RowKey::Connect(1)], "Ctrl+Shift+C starts a connection");
        frame(&ctx, &mut keys, 10, vec![key(Key::ArrowUp, Modifiers::NONE)]);
        frame(&ctx, &mut keys, 10, vec![key(Key::ArrowUp, Modifiers::NONE)]);
        frame(&ctx, &mut keys, 10, vec![]);
        assert_eq!(keys.current, Some(0), "Up stops at the first row");
    }

    /// rev-claude-53-k finding 2: Escape closes the open palette, adds nothing, and gives the
    /// focus back to the row it was opened on (the connect form already did this).
    #[test]
    fn escape_closes_the_palette_and_adds_nothing() {
        let ctx = egui::Context::default();
        let mut keys = GraphKeys::default();
        let name = |i: usize| format!("node {i}");
        let kinds = |_: usize, _: &str| vec!["Add".to_string(), "Multiply".to_string()];
        let rows = GraphRows {
            len: 3,
            name: &name,
            selected: None,
            kinds: &kinds,
            named_outputs: false,
            read_only: false,
            input_pins: true,
            deletable: true,
            heading: "Nodes",
            noun: "node",
        };
        let run = |keys: &mut GraphKeys, events: Vec<egui::Event>| {
            let mut got = Vec::new();
            let mut out = ctx.run_ui(raw([400.0, 400.0], events), |ui| {
                got = graph_keys_ui(ui, "t", keys, &rows);
            });
            out.textures_delta.clear();
            got
        };
        run(&mut keys, vec![]);
        keys.list.focus(1);
        run(&mut keys, vec![]);
        keys.palette = Some(String::new());
        keys.focus_form = true;
        run(&mut keys, vec![]);
        assert!(keys.palette.is_some(), "the palette stays open without a key");
        let got = run(&mut keys, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(keys.palette.is_none(), "Escape closed the palette");
        assert!(got.is_empty(), "nothing was added: {got:?}");
        assert_eq!(keys.list.current, Some(1), "the focus is back on the row");
    }

    #[test]
    fn a_long_list_lays_out_only_the_rows_in_view_and_follows_the_focus() {
        let ctx = egui::Context::default();
        let mut keys = ListKeys::default();
        frame(&ctx, &mut keys, 5_000, vec![]);
        frame(&ctx, &mut keys, 5_000, vec![]);
        assert_eq!(keys.total, 5_000);
        assert!(
            keys.drawn > 0 && keys.drawn < 30,
            "{} rows laid out of 5000",
            keys.drawn
        );
        keys.focus(4_000);
        frame(&ctx, &mut keys, 5_000, vec![]);
        frame(&ctx, &mut keys, 5_000, vec![]);
        assert!(keys.row_focused, "row 4000 was scrolled to and focused");
        assert_eq!(keys.current, Some(4_000));
        assert!(keys.drawn < 30, "{} rows laid out", keys.drawn);
    }
}

/// The painted canvases of the editor without a `canvas_keys` route, each with why: the list
/// `scan_tests` checks every painted region of the crate against (ADR-7733 item 4). A file that
/// paints a canvas (`SnarlWidget::new`, `allocate_painter`, `Sense::click_and_drag()` outside its
/// tests) must use `canvas_keys` or have a row here. Classes: `view_only` (nothing to edit),
/// `not_canvas` (the "painted" region is a named widget the keyboard already reaches),
/// `own_keys` (the canvas has its own keyboard route), `pending` (FU-5 work still to do; the
/// reason names it). A row for a file that no longer paints, or that now uses `canvas_keys`,
/// fails the scan as stale.
pub const CANVAS_ALLOW: &[(&str, &str, &str)] = &[
    (
        "anim/locomotion_panel.rs",
        "view_only",
        "locomotion test bed: a hover-only drawing of the walker, its ground and the gizmo overlay; nothing \
         on it is clicked or dragged, and the panel's buttons and fields beside it are named widgets",
    ),
    (
        "anim/preview.rs",
        "view_only",
        "rig preview: orbit and joint highlight; the skeleton tree lists the joints",
    ),
    (
        "import_dialog.rs",
        "own_keys",
        "model preview: drag to orbit, or focus it and turn the model with the arrow keys; the dialog's \
         settings, clip rows and buttons are named widgets (editor.import_with drives them)",
    ),
    (
        "inspector/egui_utils.rs",
        "not_canvas",
        "painted list buttons, each a named Button (FU-6)",
    ),
    (
        "inspector/reflect_ui.rs",
        "not_canvas",
        "a field label is a frameless Button in the Tab order that also scrubs on drag (ADR-7785), and a \
         list element's grip is a focusable handle moved with Alt+Up / Alt+Down (P-122)",
    ),
    (
        "mm_db_editor/timeline.rs",
        "own_keys",
        "motion-database span timeline: a click focuses it, then Left/Right move the selected edge, Up/Down \
         pick a span, E picks start, end or body, N adds a span and Delete removes it (editor.mm_db_* methods)",
    ),
    ("montage_panel.rs", "pending", "FU-5: montage section strip"),
    (
        "outliner.rs",
        "not_canvas",
        "rows are selectable buttons the keyboard reaches (drag is reparenting)",
    ),
    (
        "procgen/widgets.rs",
        "pending",
        "FU-5: procedural curve parameter",
    ),
    (
        "seq_timeline.rs",
        "pending",
        "FU-5: sequence timeline scrub and track rows",
    ),
    ("tables_panel.rs", "pending", "FU-5: curve table plot"),
    (
        "viewport.rs",
        "own_keys",
        "viewport panes take the focus (FU-9) and have their own hotkeys",
    ),
];

#[cfg(test)]
mod scan_tests {
    //! ADR-7733 item 4: a new painted canvas without a keyboard route fails here.

    use std::path::{Path, PathBuf};

    /// What marks a painted canvas in source.
    const MARKERS: &[&str] = &[
        "SnarlWidget::new(",
        "allocate_painter(",
        "Sense::click_and_drag()",
    ];

    /// `text` without its test module (from the first `#[cfg(test)]` that opens a module).
    fn non_test(text: &str) -> &str {
        let mut at = 0;
        while let Some(i) = text[at..].find("#[cfg(test)]") {
            let start = at + i;
            let rest = text[start + "#[cfg(test)]".len()..].trim_start();
            if rest.starts_with("mod ") || rest.starts_with("pub(crate) mod ") || rest.starts_with("pub mod ")
            {
                return &text[..start];
            }
            at = start + 1;
        }
        text
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// The source files (relative, `/`-separated) that paint a canvas, and whether each uses
    /// `canvas_keys`.
    fn painted() -> Vec<(String, bool)> {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        let mut out = Vec::new();
        for path in files {
            let rel = path
                .strip_prefix(&src)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if rel == "canvas_keys.rs" || rel.ends_with("tests.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let code = non_test(&text);
            if MARKERS.iter().any(|m| code.contains(m)) {
                out.push((rel, code.contains("canvas_keys::")));
            }
        }
        out.sort();
        out
    }

    #[test]
    fn every_painted_canvas_has_a_keyboard_route_or_a_reason() {
        let painted = painted();
        assert!(
            painted
                .iter()
                .any(|(f, keyed)| f == "matgraph_editor/panel.rs" && *keyed),
            "the scan finds the material graph canvas: {painted:?}"
        );
        let mut problems = Vec::new();
        for (file, keyed) in &painted {
            let row = super::CANVAS_ALLOW.iter().find(|(f, ..)| f == file);
            match (keyed, row) {
                (false, None) => problems.push(format!(
                    "{file} paints a canvas with no canvas_keys route: give it one (canvas_keys.rs) \
                     or add a CANVAS_ALLOW row with its class and reason"
                )),
                (true, Some(_)) => problems.push(format!(
                    "{file} uses canvas_keys now: remove its CANVAS_ALLOW row"
                )),
                _ => {}
            }
        }
        for (file, class, reason) in super::CANVAS_ALLOW {
            if !painted.iter().any(|(f, _)| f == file) {
                problems.push(format!("CANVAS_ALLOW row {file} is stale: it paints no canvas"));
            }
            if !["view_only", "not_canvas", "own_keys", "pending"].contains(class) {
                problems.push(format!("CANVAS_ALLOW row {file}: unknown class {class:?}"));
            }
            if reason.trim().is_empty() {
                problems.push(format!("CANVAS_ALLOW row {file} has no reason"));
            }
        }
        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[test]
    fn the_scan_ignores_test_modules() {
        let text = "fn a() {}\n#[cfg(test)]\nfn helper() {}\n#[cfg(test)]\nmod tests { allocate_painter( }";
        assert!(!MARKERS.iter().any(|m| non_test(text).contains(m)));
        assert!(non_test(text).contains("helper"));
    }
}
