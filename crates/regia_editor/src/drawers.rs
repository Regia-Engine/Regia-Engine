// SPDX-License-Identifier: MPL-2.0
//! Drawers (ADR-7754): the Content Browser and the Output Log slide up from the bottom edge over
//! the dock area (about [`HEIGHT_FRACTION`] of its height) without changing the dock tree.
//!
//! - **Open.** `view.content_drawer` (Ctrl+Shift+Space, rebindable in the palette with F2),
//!   `view.log_drawer`, the status bar's Content and Log buttons, or `editor.drawer { drawer, open }`.
//! - **Close.** Esc, the drawer's Close button, a press outside it (the status bar excepted: its
//!   buttons toggle), or the keyboard focus moving to a widget outside it ([`focus_left`]).
//! - **Dock.** The drawer's Dock button (and `editor.drawer { dock: true }`) closes it and opens the
//!   panel as a normal tab through the layout requests (the palette's `window.open_*` route).
//!
//! A drawer draws the panel's own state: the content drawer is the Content Browser (same folder and
//! filters as its tab), the log drawer is the Output Log with the console line under it
//! ([`crate::console`]). A closed drawer draws nothing and requests no repaint.

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// The share of the dock area's height an open drawer covers.
pub const HEIGHT_FRACTION: f32 = 0.4;
/// The palette command that toggles the content drawer.
pub const CONTENT_COMMAND: &str = "view.content_drawer";
/// The palette command that toggles the log drawer.
pub const LOG_COMMAND: &str = "view.log_drawer";
/// The palette command that docks the open drawer as a tab.
pub const DOCK_COMMAND: &str = "view.dock_drawer";
/// The salt of the log drawer's console line (`log_panel::output_log_with_console`).
pub const DRAWER_LINE: &str = "drawer";

/// One drawer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawerKind {
    /// The Content Browser.
    Content,
    /// The Output Log and the console line.
    Log,
}

impl DrawerKind {
    /// Both drawers.
    pub const ALL: [Self; 2] = [Self::Content, Self::Log];

    /// The protocol name (`content`, `log`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::Log => "log",
        }
    }

    /// The drawer named `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// The panel a Dock turns it into.
    #[must_use]
    pub fn panel(self) -> crate::shell::Panel {
        match self {
            Self::Content => crate::shell::Panel::Content,
            Self::Log => crate::shell::Panel::OutputLog,
        }
    }

    /// The drawer's title.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Content => "Content",
            Self::Log => "Output Log",
        }
    }
}

/// Which drawer is open (a world resource: the shell, the palette and `editor.drawer` share it).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct Drawers {
    open: Option<DrawerKind>,
    /// The focused widget the last drawn frame saw.
    last_focus: Option<egui::Id>,
    /// The frame whose keys were handled (egui may run a frame's UI twice).
    keys_frame: Option<u64>,
    /// Move the keyboard focus onto the drawer's Dock button when it next draws (a content
    /// drawer that just opened; the log drawer focuses its console line instead).
    focus_header: bool,
    /// Times a drawer opened (a counter for tests).
    pub opened: u64,
}

impl Drawers {
    /// The open drawer.
    #[must_use]
    pub fn open(&self) -> Option<DrawerKind> {
        self.open
    }

    /// Opens or closes `kind` (opening one closes the other).
    pub fn set(&mut self, kind: DrawerKind, open: bool) {
        if open {
            if self.open != Some(kind) {
                self.opened += 1;
                self.last_focus = None;
                self.focus_header = kind == DrawerKind::Content;
            }
            self.open = Some(kind);
        } else if self.open == Some(kind) {
            self.open = None;
        }
    }

    /// Opens `kind`, or closes it when it is open. Answers whether it is open now.
    pub fn toggle(&mut self, kind: DrawerKind) -> bool {
        let open = self.open != Some(kind);
        self.set(kind, open);
        open
    }

    /// Closes whichever drawer is open.
    pub fn close(&mut self) {
        self.open = None;
    }
}

/// Toggles `kind` in `world` (the palette's `view.content_drawer` / `view.log_drawer`). Opening the
/// log drawer focuses its console line. Answers the status line.
pub fn toggle(world: &mut World, kind: DrawerKind) -> String {
    let open = world.get_resource_or_insert_with(Drawers::default).toggle(kind);
    if open && kind == DrawerKind::Log {
        // The drawer's own line ("drawer", as the shell draws it): the docked Output Log tab's
        // line draws first and must not take the focus out of the drawer.
        world
            .get_resource_or_insert_with(crate::console::ConsoleState::default)
            .focus_line(DRAWER_LINE);
    }
    format!("{} drawer {}", kind.title(), if open { "open" } else { "closed" })
}

/// Docks the open drawer (the palette's `view.dock_drawer`). Answers the status line.
pub fn dock_open(world: &mut World) -> String {
    match world.get_resource::<Drawers>().and_then(Drawers::open) {
        Some(kind) => {
            dock(world, kind);
            format!("{} docked as a tab", kind.title())
        }
        None => "No drawer is open".to_string(),
    }
}

/// Closes `kind` and opens its panel as a dock tab (the Dock button, `editor.drawer { dock }`).
pub fn dock(world: &mut World, kind: DrawerKind) {
    world
        .get_resource_or_insert_with(Drawers::default)
        .set(kind, false);
    crate::editor_layout::LayoutRequests::push(
        world,
        crate::editor_layout::LayoutRequest::Open(kind.panel()),
    );
}

/// Handles the drawer keys once per frame: Ctrl+Shift+Space toggles the content drawer unless the
/// user bound `view.content_drawer` to another chord in the palette (their chord runs it there);
/// like the palette's user shortcuts it does nothing during play or while a text field has the
/// focus. Esc closes the open drawer.
pub fn handle_keys(ctx: &egui::Context, world: &mut World) {
    let frame = ctx.cumulative_frame_nr();
    let mut drawers = world.get_resource_or_insert_with(Drawers::default);
    if drawers.keys_frame == Some(frame) {
        return;
    }
    // Bookkeeping only: an idle frame must not mark the resource changed.
    drawers.bypass_change_detection().keys_frame = Some(frame);
    let open = drawers.open.is_some();
    let rebound = world
        .get_resource::<crate::command_palette::CommandKeymap>()
        .is_some_and(|keymap| keymap.bindings.contains_key(CONTENT_COMMAND));
    let playing = world
        .get_resource::<crate::pie::PieStatus>()
        .is_some_and(|status| status.mode.is_some());
    let toggle_content = !rebound
        && !playing
        && !ctx.text_edit_focused()
        && ctx.input_mut(|i| {
            i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::Space,
            ))
        });
    if open && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        world.resource_mut::<Drawers>().close();
    }
    if toggle_content {
        let _ = toggle(world, DrawerKind::Content);
    }
}

/// What a raw Escape the window host sees does (main.rs): an open drawer keeps Escape (its own
/// key closes it), like a focused panel, so the same press never also stops play or asks to close
/// the editor; otherwise [`crate::shell::escape_action`].
#[must_use]
pub fn window_escape(ctx: &egui::Context, world: &World, session_active: bool) -> crate::shell::EscapeAction {
    crate::shell::escape_action(
        session_active,
        drawer_open(world) || ctx.egui_wants_keyboard_input(),
    )
}

/// Whether a drawer is open (it takes Escape before the window does; the editor binary combines
/// it with the full-screen rule, `workspaces::escape_action_full`).
#[must_use]
pub fn drawer_open(world: &World) -> bool {
    world.get_resource::<Drawers>().is_some_and(|d| d.open.is_some())
}

/// Whether a focus change to `focused` leaves the drawer whose layer is `drawer`: the focused
/// widget sits on the dock, a panel or a window (`Background` or `Middle` layers) rather than on
/// the drawer or a popup it opened. Unknown widgets (not drawn yet) do not count.
#[must_use]
pub fn focus_left(ctx: &egui::Context, focused: egui::Id, drawer: egui::LayerId) -> bool {
    ctx.read_response(focused)
        .is_some_and(|r| r.layer_id != drawer && r.layer_id.order < egui::Order::Foreground)
}

/// Whether the content drawer is open, so the docked Content tab (instance 0, which shares the
/// drawer's state and its widget ids, such as the `content-folders` pane) draws a note instead of
/// the browser (ADR-8702). The drawer draws after the dock and every way to open it (keys, palette,
/// status bar, `editor.drawer`) runs before the dock, so the browser draws in one place per pass.
#[must_use]
pub fn content_in_drawer(world: &World) -> bool {
    world.get_resource::<Drawers>().and_then(Drawers::open) == Some(DrawerKind::Content)
}

/// What the docked Content tab shows while [`content_in_drawer`].
pub fn docked_content_note(ui: &mut egui::Ui) {
    ui.weak("The Content Browser is open in the drawer. Close the drawer (Esc) to browse here.");
}

/// How often the first Content Browser (instance 0: the docked tab or the content drawer) drew in
/// one egui pass (ADR-8702). Both copies share widget ids, so more than one draw in a pass is a
/// bug (a debug assertion in egui); a counter for tests.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContentDraws {
    /// The egui pass (`Context::cumulative_pass_nr`) the count is for.
    pub pass: u64,
    /// The draws in that pass.
    pub count: u32,
    /// The most draws any one pass saw.
    pub max: u32,
}

/// Counts one draw of the first Content Browser (`content::content_browser_panel` calls it).
pub fn note_content_draw(ctx: &egui::Context, world: &mut World) {
    if crate::panel_instances::drawing_instance(world) != 0 {
        return;
    }
    let pass = ctx.cumulative_pass_nr();
    let mut draws = world.get_resource_or_insert_with(ContentDraws::default);
    // Bookkeeping only: drawing must not mark the resource changed.
    let draws = draws.bypass_change_detection();
    if draws.pass != pass {
        draws.pass = pass;
        draws.count = 0;
    }
    draws.count += 1;
    draws.max = draws.max.max(draws.count);
}

/// What the drawer did this frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrawerFrame {
    /// The Dock button was pressed for this drawer.
    pub dock: Option<DrawerKind>,
    /// The drawer closed this frame (Close, a press outside, a focus change).
    pub closed: bool,
}

/// Draws the open drawer over `dock` (the dock area's rectangle): its header (title, Dock, Close)
/// and `contents`. `status` is the status bar's rectangle (presses there do not close it).
pub fn show(
    ctx: &egui::Context,
    world: &mut World,
    dock: egui::Rect,
    status: egui::Rect,
    mut contents: impl FnMut(&mut egui::Ui, &mut World, DrawerKind),
) -> DrawerFrame {
    let mut out = DrawerFrame::default();
    let Some(kind) = world.get_resource::<Drawers>().and_then(Drawers::open) else {
        return out;
    };
    let _span = tracing::info_span!(target: "regia_editor", "drawer", kind = kind.name()).entered();
    let height = (dock.height() * HEIGHT_FRACTION).clamp(dock.height().min(120.0), dock.height());
    let area_id = egui::Id::new(("regia-drawer", kind.name()));
    // The slide: from the bottom edge up to the drawer's height over the animation time.
    let shown = ctx.animate_bool(area_id.with("slide"), true);
    let top = dock.bottom() - height * shown;
    let rect = egui::Rect::from_min_size(egui::pos2(dock.left(), top), egui::vec2(dock.width(), height));
    let mut close = false;
    let focus_header = std::mem::take(&mut world.resource_mut::<Drawers>().focus_header);
    let area = egui::Area::new(area_id)
        .order(egui::Order::Foreground)
        .fixed_pos(rect.min)
        .constrain(false)
        .show(ctx, |ui| {
            ui.set_clip_rect(rect.intersect(dock));
            egui::Frame::window(ui.style()).show(ui, |ui| {
                let inner = rect.shrink(ui.spacing().window_margin.sum().x / 2.0);
                ui.set_min_size(inner.size());
                ui.set_max_size(inner.size());
                ui.horizontal(|ui| {
                    ui.strong(kind.title());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if crate::agent_parity::action_button(ui, &crate::agent_parity::DRAWER_CLOSE)
                            .on_hover_text("Close the drawer (Esc)")
                            .clicked()
                        {
                            close = true;
                        }
                        let dock_button =
                            crate::agent_parity::action_button(ui, &crate::agent_parity::DRAWER_DOCK)
                                .on_hover_text("Keep it open as a tab in the dock");
                        if focus_header {
                            dock_button.request_focus();
                        }
                        if dock_button.clicked() {
                            out.dock = Some(kind);
                        }
                    });
                });
                ui.separator();
                // Salted ids: the panel may show as a tab at the same time.
                ui.push_id(("regia-drawer-contents", kind.name()), |ui| {
                    contents(ui, world, kind)
                });
            });
        });
    let layer = area.response.layer_id;
    // A press outside the drawer (not on the status bar, not on a popup) closes it.
    let press = ctx.input(|i| {
        i.pointer
            .any_pressed()
            .then(|| i.pointer.press_origin())
            .flatten()
    });
    if let Some(pos) = press
        && !rect.contains(pos)
        && !status.contains(pos)
        && ctx
            .layer_id_at(pos)
            .is_none_or(|at| at != layer && at.order < egui::Order::Foreground)
    {
        close = true;
    }
    let mut drawers = world.get_resource_or_insert_with(Drawers::default).clone();
    let focused = ctx.memory(|m| m.focused());
    if focused != drawers.last_focus {
        if focused.is_some_and(|id| focus_left(ctx, id, layer)) {
            close = true;
        }
        drawers.last_focus = focused;
    }
    if let Some(kind) = out.dock {
        drawers.set(kind, false);
        crate::editor_layout::LayoutRequests::push(
            world,
            crate::editor_layout::LayoutRequest::Open(kind.panel()),
        );
    } else if close {
        drawers.set(kind, false);
        out.closed = true;
    }
    world.insert_resource(drawers);
    out
}

// ---------------------------------------------------------------------------------------------
// Agent method

/// `editor.drawer { drawer, open?, dock? }`.
fn drawer_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = params.get("drawer").and_then(Value::as_str).unwrap_or_default();
    let Some(kind) = DrawerKind::parse(name) else {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "bad_drawer",
            "`drawer` is \"content\" or \"log\"",
            "see book/agents/editor-console.md",
        ));
    };
    let flag = |key: &str| match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(RpcError::new(
            INVALID_PARAMS,
            "bad_param",
            format!("`{key}` must be a boolean"),
            "see book/agents/editor-console.md",
        )),
    };
    let (open, docked) = (flag("open")?, flag("dock")?.unwrap_or(false));
    if docked {
        dock(world, kind);
    } else if let Some(open) = open {
        let now = world.get_resource_or_insert_with(Drawers::default).open() == Some(kind);
        if now != open {
            let _ = toggle(world, kind);
        }
    }
    let state = world.get_resource_or_insert_with(Drawers::default).open();
    Ok(json!({
        "drawer": kind.name(),
        "open": state == Some(kind),
        "docked": docked,
        "open_drawer": state.map(DrawerKind::name),
    }))
}

/// The drawers' agent method, installed by `agent_ext::all_methods`.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.drawer",
            tool: "editor_drawer",
            description: "The content and log drawers that slide up over the dock area: open or close one (opening one closes the other; the log drawer focuses its console line), or dock it as a normal tab. Without open or dock it reports the state.",
            params: &[
                ParamSchema {
                    name: "drawer",
                    kind: "string",
                    required: true,
                    description: "\"content\" or \"log\".",
                },
                ParamSchema {
                    name: "open",
                    kind: "boolean",
                    required: false,
                    description: "Open (true) or close (false) the drawer.",
                },
                ParamSchema {
                    name: "dock",
                    kind: "boolean",
                    required: false,
                    description: "true closes the drawer and opens its panel as a dock tab.",
                },
            ],
            result: "{ drawer, open, docked, open_drawer: \"content\" | \"log\" | null }",
            mutating: true,
            permission: Permission::EditScene,
        },
        drawer_method as Handler,
    )]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const SCREEN: egui::Rect = egui::Rect {
        min: egui::Pos2::ZERO,
        max: egui::pos2(1000.0, 700.0),
    };
    const DOCK: egui::Rect = egui::Rect {
        min: egui::pos2(0.0, 40.0),
        max: egui::pos2(1000.0, 660.0),
    };
    const STATUS: egui::Rect = egui::Rect {
        min: egui::pos2(0.0, 660.0),
        max: egui::pos2(1000.0, 700.0),
    };

    struct Frame {
        drawn: bool,
        out: DrawerFrame,
        outside: egui::Id,
        inside: Option<egui::Id>,
        repaint: bool,
    }

    /// One headless frame: a dock stand-in with one focusable button, and the drawer over it with
    /// one text field.
    fn frame(ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>) -> Frame {
        let mut drawn = false;
        let mut out = DrawerFrame::default();
        let mut outside = egui::Id::NULL;
        let mut inside = None;
        let raw = egui::RawInput {
            screen_rect: Some(SCREEN),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                outside = ui.button("dock widget").id;
            });
            handle_keys(ui.ctx(), world);
            out = show(ui.ctx(), world, DOCK, STATUS, |ui, _, _| {
                drawn = true;
                let mut text = String::new();
                inside = Some(ui.text_edit_singleline(&mut text).id);
            });
        });
        output.textures_delta.clear();
        let repaint = output
            .viewport_output
            .values()
            .any(|v| v.repaint_delay < std::time::Duration::from_secs(1));
        Frame {
            drawn,
            out,
            outside,
            inside,
            repaint,
        }
    }

    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<Drawers>();
        world
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn the_shortcut_toggles_the_content_drawer_and_escape_closes_it() {
        let ctx = egui::Context::default();
        let mut world = world();
        let ctrl_shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
        assert!(!frame(&ctx, &mut world, vec![]).drawn);
        let f = frame(&ctx, &mut world, vec![key(egui::Key::Space, ctrl_shift)]);
        assert!(f.drawn, "Ctrl+Shift+Space opens it");
        assert_eq!(world.resource::<Drawers>().open(), Some(DrawerKind::Content));
        frame(&ctx, &mut world, vec![key(egui::Key::Space, ctrl_shift)]);
        assert_eq!(
            world.resource::<Drawers>().open(),
            None,
            "the same chord closes it"
        );
        frame(&ctx, &mut world, vec![key(egui::Key::Space, ctrl_shift)]);
        frame(
            &ctx,
            &mut world,
            vec![key(egui::Key::Escape, egui::Modifiers::NONE)],
        );
        assert_eq!(world.resource::<Drawers>().open(), None, "Esc closes it");
        assert_eq!(world.resource::<Drawers>().opened, 2);
    }

    #[test]
    fn a_rebound_command_turns_the_built_in_chord_off() {
        let ctx = egui::Context::default();
        let mut world = world();
        let mut keymap = crate::command_palette::CommandKeymap::default();
        keymap
            .bind(
                CONTENT_COMMAND,
                crate::command_palette::KeyChord::parse("Ctrl+Shift+K").unwrap(),
            )
            .unwrap();
        world.insert_resource(keymap);
        let ctrl_shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
        frame(&ctx, &mut world, vec![key(egui::Key::Space, ctrl_shift)]);
        assert_eq!(world.resource::<Drawers>().open(), None);
        // The user's chord runs the palette command, which toggles it.
        assert_eq!(
            crate::command_palette::run(&mut world, CONTENT_COMMAND).as_deref(),
            Some("Content drawer open")
        );
        assert_eq!(world.resource::<Drawers>().open(), Some(DrawerKind::Content));
    }

    #[test]
    fn focus_moving_outside_closes_the_drawer_and_focus_inside_keeps_it() {
        let ctx = egui::Context::default();
        let mut world = world();
        world.resource_mut::<Drawers>().set(DrawerKind::Log, true);
        let f = frame(&ctx, &mut world, vec![]);
        let inside = f.inside.unwrap();
        // Focus inside the drawer keeps it open.
        ctx.memory_mut(|m| m.request_focus(inside));
        frame(&ctx, &mut world, vec![]);
        let f = frame(&ctx, &mut world, vec![]);
        assert!(f.drawn);
        assert_eq!(world.resource::<Drawers>().open(), Some(DrawerKind::Log));
        // A synthetic focus change to the dock's widget closes it.
        ctx.memory_mut(|m| m.request_focus(f.outside));
        frame(&ctx, &mut world, vec![]);
        let f = frame(&ctx, &mut world, vec![]);
        assert!(!f.drawn);
        assert_eq!(world.resource::<Drawers>().open(), None);
    }

    #[test]
    fn a_press_outside_closes_it_but_not_on_the_status_bar() {
        let ctx = egui::Context::default();
        let mut world = world();
        world.resource_mut::<Drawers>().set(DrawerKind::Content, true);
        frame(&ctx, &mut world, vec![]);
        let press = |pos: egui::Pos2| {
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        frame(&ctx, &mut world, press(egui::pos2(500.0, 680.0)));
        assert_eq!(
            world.resource::<Drawers>().open(),
            Some(DrawerKind::Content),
            "status bar"
        );
        frame(&ctx, &mut world, press(egui::pos2(500.0, 600.0)));
        assert_eq!(
            world.resource::<Drawers>().open(),
            Some(DrawerKind::Content),
            "inside"
        );
        let f = frame(&ctx, &mut world, press(egui::pos2(500.0, 100.0)));
        assert!(f.out.closed);
        assert_eq!(world.resource::<Drawers>().open(), None, "the dock above");
    }

    fn click(pos: egui::Pos2) -> Vec<egui::Event> {
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        vec![egui::Event::PointerMoved(pos), button(true), button(false)]
    }

    /// Esc on an open drawer is the drawer's: the window host does not also stop play or close
    /// the editor when no widget has the focus (review w12-rev-early-k row 2).
    #[test]
    fn the_window_host_leaves_escape_to_an_open_drawer() {
        use crate::shell::EscapeAction;
        let ctx = egui::Context::default();
        let mut world = world();
        let ctrl_shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
        frame(&ctx, &mut world, vec![key(egui::Key::Space, ctrl_shift)]);
        frame(&ctx, &mut world, vec![]);
        // A click on the drawer's empty space drops the focus (egui surrenders it on a click).
        frame(&ctx, &mut world, click(egui::pos2(500.0, 600.0)));
        frame(&ctx, &mut world, vec![]);
        assert_eq!(world.resource::<Drawers>().open(), Some(DrawerKind::Content));
        assert_eq!(ctx.memory(|m| m.focused()), None, "no widget has the focus");
        assert_eq!(window_escape(&ctx, &world, false), EscapeAction::Ignore);
        assert_eq!(window_escape(&ctx, &world, true), EscapeAction::Ignore);
        frame(
            &ctx,
            &mut world,
            vec![key(egui::Key::Escape, egui::Modifiers::NONE)],
        );
        assert_eq!(world.resource::<Drawers>().open(), None);
        assert_eq!(window_escape(&ctx, &world, false), EscapeAction::Close);
        assert_eq!(window_escape(&ctx, &world, true), EscapeAction::StopPlay);
    }

    /// The log drawer opened while the Output Log tab shows in the dock stays open: its own
    /// console line takes the focus, not the docked tab's (review w12-rev-early-k row 4).
    #[test]
    fn the_log_drawer_stays_open_over_a_visible_output_log_tab() {
        let ctx = egui::Context::default();
        let mut world = world();
        let mut tab = crate::log_panel::LogPanelState::default();
        let mut drawer = crate::log_panel::LogPanelState::default();
        let mut run = |world: &mut World| {
            let raw = egui::RawInput {
                screen_rect: Some(SCREEN),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let _ = crate::log_panel::output_log_with_console(ui, &mut tab, world, "tab");
                });
                handle_keys(ui.ctx(), world);
                let _ = show(ui.ctx(), world, DOCK, STATUS, |ui, world, _| {
                    let _ = crate::log_panel::output_log_with_console(ui, &mut drawer, world, DRAWER_LINE);
                });
            });
            output.textures_delta.clear();
        };
        run(&mut world);
        run(&mut world);
        assert_eq!(toggle(&mut world, DrawerKind::Log), "Output Log drawer open");
        for _ in 0..3 {
            run(&mut world);
        }
        assert_eq!(world.resource::<Drawers>().open(), Some(DrawerKind::Log));
        let drawer_line = egui::Id::new(("regia-console-line", DRAWER_LINE));
        assert!(
            ctx.memory(|m| m.has_focus(drawer_line)),
            "the drawer's line has the focus"
        );
    }

    /// Ctrl+Shift+Space does nothing during play or while a text field has the focus, as the
    /// palette's user shortcuts (review w12-rev-early-k row 21).
    #[test]
    fn the_chord_is_skipped_during_play_and_text_entry() {
        let ctx = egui::Context::default();
        let mut world = world();
        let ctrl_shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
        world.insert_resource(crate::pie::PieStatus {
            mode: Some(crate::pie::PlayMode::Playing),
            ..Default::default()
        });
        frame(&ctx, &mut world, vec![key(egui::Key::Space, ctrl_shift)]);
        assert_eq!(world.resource::<Drawers>().open(), None, "during play");
        world.resource_mut::<crate::pie::PieStatus>().mode = None;
        let field = egui::Id::new("typing");
        let run = |world: &mut World, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(SCREEN),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let mut text = String::new();
                    ui.add(egui::TextEdit::singleline(&mut text).id(field));
                });
                handle_keys(ui.ctx(), world);
            });
            output.textures_delta.clear();
        };
        run(&mut world, vec![]);
        ctx.memory_mut(|m| m.request_focus(field));
        run(&mut world, vec![]);
        run(&mut world, vec![key(egui::Key::Space, ctrl_shift)]);
        assert_eq!(world.resource::<Drawers>().open(), None, "while typing");
        ctx.memory_mut(|m| m.surrender_focus(field));
        run(&mut world, vec![]);
        run(&mut world, vec![key(egui::Key::Space, ctrl_shift)]);
        assert_eq!(
            world.resource::<Drawers>().open(),
            Some(DrawerKind::Content),
            "otherwise it opens"
        );
    }

    #[test]
    fn toggling_leaves_the_dock_tree_alone_and_dock_adds_the_tab() {
        let mut world = world();
        let shell = crate::shell::EditorShell::new(None, None);
        let before = format!("{:?}", shell.layout());
        assert_eq!(toggle(&mut world, DrawerKind::Content), "Content drawer open");
        assert_eq!(toggle(&mut world, DrawerKind::Log), "Output Log drawer open");
        assert_eq!(
            world.resource::<Drawers>().open(),
            Some(DrawerKind::Log),
            "one at a time"
        );
        assert_eq!(format!("{:?}", shell.layout()), before);
        assert!(!world.contains_resource::<crate::editor_layout::LayoutRequests>());
        dock(&mut world, DrawerKind::Log);
        assert_eq!(world.resource::<Drawers>().open(), None);
        assert_eq!(
            world.resource::<crate::editor_layout::LayoutRequests>().pending,
            vec![crate::editor_layout::LayoutRequest::Open(
                crate::shell::Panel::OutputLog
            )]
        );
    }

    /// The keyboard path: Ctrl+Shift+Space opens the content drawer with its Dock button focused,
    /// Enter presses it; Tab moves on to Close, Enter closes.
    #[test]
    fn the_header_buttons_work_from_the_keyboard() {
        let ctx = egui::Context::default();
        let mut world = world();
        let ctrl_shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
        frame(&ctx, &mut world, vec![key(egui::Key::Space, ctrl_shift)]);
        frame(&ctx, &mut world, vec![]);
        let f = frame(
            &ctx,
            &mut world,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        assert_eq!(
            f.out.dock,
            Some(DrawerKind::Content),
            "Enter on the focused Dock button"
        );
        assert_eq!(world.resource::<Drawers>().open(), None);
        assert_eq!(
            world.resource::<crate::editor_layout::LayoutRequests>().pending,
            vec![crate::editor_layout::LayoutRequest::Open(
                crate::shell::Panel::Content
            )]
        );
        // The palette route for the log drawer, whose console line keeps Tab for completion.
        world.resource_mut::<Drawers>().set(DrawerKind::Log, true);
        assert_eq!(dock_open(&mut world), "Output Log docked as a tab");
        assert_eq!(dock_open(&mut world), "No drawer is open");
    }

    #[test]
    fn a_closed_drawer_draws_nothing_and_asks_for_no_repaint() {
        let ctx = egui::Context::default();
        let mut world = world();
        // A fresh context repaints while its first frames settle; then a closed drawer adds nothing.
        for _ in 0..3 {
            assert!(!frame(&ctx, &mut world, vec![]).drawn);
        }
        for _ in 0..3 {
            let f = frame(&ctx, &mut world, vec![]);
            assert!(!f.drawn);
            assert!(!f.repaint);
        }
    }

    /// The editor-smoke step `command:view.content_drawer` (lane v06-editor-smoke-fixes,
    /// ADR-8702): the content drawer opened over a shown Content tab drew the browser twice in one
    /// pass, and egui's debug build panicked on the `content-folders` resize handle changing layer.
    /// In a headless editor shell with Content docked and in front: the browser draws once per
    /// pass with the drawer closed, open and closed again, and the folder pane lives on the
    /// drawer's layer while it is open. Debug test builds keep egui's id check on.
    #[test]
    fn the_content_drawer_over_a_docked_content_tab_draws_the_browser_once() {
        let dir = regia_test_utils::unique_temp_dir("drawer-content-docked");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"D\"\nid = \"com.example.d\"\n",
        )
        .unwrap();
        let mut app = regia_app::prelude::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let open = crate::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        let mut shell = crate::shell::EditorShell::new(None, Some(dir.clone()));
        shell.open_panel(crate::shell::Panel::Content);
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 1000.0));
        let mut frame = 0_u32;
        let mut pass = |shell: &mut crate::shell::EditorShell, world: &mut World| {
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                time: Some(f64::from(frame) / 60.0),
                ..Default::default()
            };
            frame += 1;
            let mut output = ctx.run_ui(raw, |ui| shell.ui(ui, world, None));
            output.textures_delta.clear();
        };
        let folders = egui::Id::new("content-folders").with("__resize");
        let drawer_layer = egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new(("regia-drawer", "content")),
        );
        // The count of the pass that just ran (a pass with no draw would leave an older one).
        let draws = |world: &World| {
            let draws = *world.resource::<ContentDraws>();
            assert_eq!(
                draws.pass + 1,
                ctx.cumulative_pass_nr(),
                "a draw in the last pass"
            );
            draws
        };
        for _ in 0..3 {
            pass(&mut shell, app.world_mut());
        }
        assert_eq!(
            draws(app.world()).count,
            1,
            "the docked Content tab draws the browser"
        );
        let docked_layer = ctx.read_response(folders).map(|r| r.layer_id);
        assert!(
            docked_layer.is_some_and(|layer| layer != drawer_layer),
            "the folder pane is the docked tab's: {docked_layer:?}"
        );
        assert_eq!(
            crate::command_palette::run(app.world_mut(), CONTENT_COMMAND).as_deref(),
            Some("Content drawer open")
        );
        for _ in 0..4 {
            pass(&mut shell, app.world_mut());
        }
        assert_eq!(
            app.world().resource::<Drawers>().open(),
            Some(DrawerKind::Content)
        );
        assert_eq!(
            draws(app.world()).count,
            1,
            "the drawer draws it, the tab gives way"
        );
        assert_eq!(
            ctx.read_response(folders).map(|r| r.layer_id),
            Some(drawer_layer),
            "the folder pane is the drawer's"
        );
        let _ = toggle(app.world_mut(), DrawerKind::Content);
        for _ in 0..2 {
            pass(&mut shell, app.world_mut());
        }
        assert_eq!(draws(app.world()).count, 1, "the tab draws it again");
        assert_eq!(ctx.read_response(folders).map(|r| r.layer_id), docked_layer);
        assert_eq!(draws(app.world()).max, 1, "never two draws in one pass");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_agent_method_opens_closes_and_docks() {
        let mut world = world();
        let out = drawer_method(&mut world, &json!({ "drawer": "log", "open": true })).unwrap();
        assert_eq!(out["open"], true);
        assert_eq!(out["open_drawer"], "log");
        assert!(!world.resource::<crate::console::ConsoleState>().has_pending());
        let out = drawer_method(&mut world, &json!({ "drawer": "content", "open": true })).unwrap();
        assert_eq!(out["open_drawer"], "content");
        let out = drawer_method(&mut world, &json!({ "drawer": "content", "open": false })).unwrap();
        assert_eq!(out["open_drawer"], Value::Null);
        let out = drawer_method(&mut world, &json!({ "drawer": "content", "dock": true })).unwrap();
        assert_eq!(out["docked"], true);
        assert_eq!(
            world.resource::<crate::editor_layout::LayoutRequests>().pending,
            vec![crate::editor_layout::LayoutRequest::Open(
                crate::shell::Panel::Content
            )]
        );
        let err = drawer_method(&mut world, &json!({ "drawer": "side" })).unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "bad_drawer");
        let err = drawer_method(&mut world, &json!({ "drawer": "log", "open": 1 })).unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "bad_param");
    }
}
