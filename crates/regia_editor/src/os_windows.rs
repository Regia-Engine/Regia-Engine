// SPDX-License-Identifier: MPL-2.0
//! Floated panels in their own OS windows (ADR-7821; parity rows P-001, P-012, P-015).
//!
//! A floated dock panel is an egui_dock window surface of the editor's one `DockState`. With
//! `ui.floating_windows = os` and a runner that opens windows ([`OsWindowSupport`]), every such
//! surface is announced each root pass as an egui deferred viewport ([`viewport_id`]); the editor
//! binary opens one winit window per viewport egui lists and runs that window's pass with
//! `EditorShell::os_window_ui`, which draws only that surface. The main window's dock draws with
//! the OS-window surfaces parked ([`OsWindows::park`]), so a panel draws in exactly one window per
//! frame. `in_window` (single-window mode) keeps the floated panels as egui windows inside the
//! main window (ADR-2771).
//!
//! Agent methods: `editor.panel_float` and `editor.panel_instances`
//! (`book/agents/editor-os-windows.md`).

use std::collections::BTreeMap;

use egui_dock::{DockState, Surface, SurfaceIndex};
use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::shell::Panel;

/// The editor preference that picks where floated panels go.
pub const FLOATING_WINDOWS_PREF: &str = "ui.floating_windows";

/// The values of [`FLOATING_WINDOWS_PREF`].
pub const FLOAT_MODE_CHOICES: &[&str] = &["os", "in_window"];

/// The default of [`FLOATING_WINDOWS_PREF`]: own OS windows on the Linux and Windows desktop,
/// single-window elsewhere.
pub const DEFAULT_FLOAT_MODE: &str = if cfg!(any(target_os = "linux", target_os = "windows")) {
    "os"
} else {
    "in_window"
};

/// A new OS window's size when nothing remembers one, points.
pub const DEFAULT_WINDOW_SIZE: [f32; 2] = [480.0, 360.0];

/// Frames a drop carried out of its window waits for the pointer to enter another window.
pub const DROP_CARRY_FRAMES: u64 = 120;

/// The part of a restored window that must be on some monitor, physical pixels (else it
/// re-docks: its monitor is gone).
pub const MIN_VISIBLE: [i32; 2] = [48, 24];

/// Where floated panels go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatMode {
    /// Each floated panel group is its own OS window.
    Os,
    /// Floated panels are windows inside the main editor window (single-window mode).
    InWindow,
}

impl FloatMode {
    /// The preference value.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Os => "os",
            Self::InWindow => "in_window",
        }
    }

    /// Parses a preference value.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "os" => Some(Self::Os),
            "in_window" => Some(Self::InWindow),
            _ => None,
        }
    }

    /// The mode the preferences in `world` ask for (the default without preferences).
    #[must_use]
    pub fn preferred(world: &World) -> Self {
        let value = world
            .get_resource::<crate::editor_prefs::EditorPreferences>()
            .and_then(|prefs| prefs.get(FLOATING_WINDOWS_PREF));
        match value {
            Some(crate::editor_prefs::PrefValue::Text(text)) => Self::from_id(&text),
            _ => None,
        }
        .or_else(|| Self::from_id(DEFAULT_FLOAT_MODE))
        .unwrap_or(Self::InWindow)
    }
}

/// Inserted by a runner that opens OS windows for egui's viewports (the editor binary). Without
/// it the effective mode is always [`FloatMode::InWindow`].
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct OsWindowSupport {
    /// The monitors' rectangles `(x, y, width, height)`, physical pixels (empty: unknown).
    pub monitors: Vec<[i32; 4]>,
}

/// Whether floated panels are OS windows this frame: the preference asks for it, a runner opens
/// windows and egui does not embed viewports.
#[must_use]
pub fn os_mode_active(ctx: &egui::Context, world: &World) -> bool {
    world.contains_resource::<OsWindowSupport>()
        && !ctx.embed_viewports()
        && FloatMode::preferred(world) == FloatMode::Os
}

/// The egui viewport id of the OS window with serial `serial`.
#[must_use]
pub fn viewport_id(serial: u64) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(("regia.os_window", serial))
}

/// Whether a window at `pos` with `size` (physical pixels) shows at least [`MIN_VISIBLE`] of
/// itself on one of `monitors` (`(x, y, width, height)`). True when no monitor is known.
#[must_use]
pub fn visible_on_monitors(pos: [i32; 2], size: [i32; 2], monitors: &[[i32; 4]]) -> bool {
    if monitors.is_empty() {
        return true;
    }
    monitors.iter().any(|&[mx, my, mw, mh]| {
        let left = pos[0].max(mx);
        let top = pos[1].max(my);
        let right = (pos[0].saturating_add(size[0])).min(mx.saturating_add(mw));
        let bottom = (pos[1].saturating_add(size[1])).min(my.saturating_add(mh));
        right - left >= MIN_VISIBLE[0] && bottom - top >= MIN_VISIBLE[1]
    })
}

/// Whether `pos` (window pixels) lies outside a window of `size` pixels.
#[must_use]
pub fn outside_window(pos: [f64; 2], size: [u32; 2]) -> bool {
    pos[0] < 0.0 || pos[1] < 0.0 || pos[0] >= f64::from(size[0]) || pos[1] >= f64::from(size[1])
}

// ---------------------------------------------------------------------------------------------
// Requests and status

/// A change to the floated panels, queued for the shell's next frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsWindowRequest {
    /// Float `panel` in its own window.
    Float(Panel),
    /// Dock `panel` back into the main window.
    Dock(Panel),
    /// Dock every panel of OS window `serial` back (its close button, or its monitor is gone).
    DockWindow(u64),
    /// Float the focused tab (`view.float_panel`).
    FloatFocused,
    /// Dock every floated panel back (`view.dock_floated_panels`).
    DockAll,
}

/// The palette command that floats the focused panel.
pub const FLOAT_COMMAND: &str = "view.float_panel";
/// The palette command that docks every floated panel back.
pub const DOCK_ALL_COMMAND: &str = "view.dock_floated_panels";

/// The palette commands of this module (`editor.command { run }`). Returns the status line, `None`
/// for another id.
pub fn run_command(world: &mut World, id: &str) -> Option<String> {
    let (request, line) = match id {
        FLOAT_COMMAND => (OsWindowRequest::FloatFocused, "Float the focused panel"),
        DOCK_ALL_COMMAND => (OsWindowRequest::DockAll, "Dock every floated panel back"),
        _ => return None,
    };
    OsWindowRequests::push(world, request);
    Some(line.to_owned())
}

/// Queued [`OsWindowRequest`]s (the shell drains them each frame).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct OsWindowRequests {
    /// In order.
    pub pending: Vec<OsWindowRequest>,
}

impl OsWindowRequests {
    /// Queues `request` in `world`.
    pub fn push(world: &mut World, request: OsWindowRequest) {
        world
            .get_resource_or_insert_with(Self::default)
            .pending
            .push(request);
    }
}

/// One floated panel group, as the shell published it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloatedWindow {
    /// The window's serial (stable while it lives).
    pub serial: u64,
    /// Its tabs.
    pub tabs: Vec<Panel>,
    /// Whether it is its own OS window (false: an egui window inside the main window).
    pub os_window: bool,
}

/// The floated windows and the mode, published by the shell each frame it changes.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct OsWindowStatus {
    /// Whether floated panels are OS windows now.
    pub os_mode: bool,
    /// Whether a runner that opens OS windows is present.
    pub supported: bool,
    /// The floated windows.
    pub windows: Vec<FloatedWindow>,
}

impl OsWindowStatus {
    /// The floated window holding `panel`.
    #[must_use]
    pub fn window_of(&self, panel: Panel) -> Option<&FloatedWindow> {
        self.windows.iter().find(|w| w.tabs.contains(&panel))
    }
}

// ---------------------------------------------------------------------------------------------
// The shell's OS-window state

/// One floated surface and its serial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    surface: SurfaceIndex,
    serial: u64,
}

/// The shell's floated-window state: a serial per floated surface, the surfaces parked while the
/// main dock draws, and each OS window's rectangle relative to the main window.
#[derive(Debug, Default)]
pub struct OsWindows {
    slots: Vec<Slot>,
    next_serial: u64,
    parked: Vec<(SurfaceIndex, Surface<Panel>)>,
    /// `(x, y, width, height)` points relative to the main window's inner top-left, by serial.
    rects: BTreeMap<u64, (f32, f32, f32, f32)>,
    /// Serials whose rectangle came from a saved layout (a window whose monitor is gone re-docks).
    restored: Vec<u64>,
    /// The main window's inner rectangle on the desktop, points (`None`: unknown, e.g. Wayland).
    main_inner: Option<egui::Rect>,
    /// The main window's pointer in the last root pass (a torn-off tab's window opens there).
    pointer: Option<egui::Pos2>,
    /// Whether the last root pass ran in OS mode.
    active: bool,
    /// The key of the last published status.
    published: Option<u64>,
}

impl OsWindows {
    /// Whether the last root pass ran in OS mode.
    #[must_use]
    pub fn active(&self) -> bool {
        self.active
    }

    /// The serials of the floated surfaces, in surface order.
    #[must_use]
    pub fn serials(&self) -> Vec<u64> {
        self.slots.iter().map(|s| s.serial).collect()
    }

    /// Whether `tab` is in an OS window (OS mode on and its surface floated).
    #[must_use]
    pub fn holds(&self, dock: &DockState<Panel>, tab: Panel) -> bool {
        self.active
            && dock
                .find_tab(&tab)
                .is_some_and(|path| self.slots.iter().any(|s| s.surface == path.surface))
    }

    /// The surface of OS window `serial`.
    #[must_use]
    pub fn surface_of(&self, serial: u64) -> Option<SurfaceIndex> {
        self.slots.iter().find(|s| s.serial == serial).map(|s| s.surface)
    }

    /// The serial of the window with egui viewport `id`.
    #[must_use]
    pub fn serial_of_viewport(&self, id: egui::ViewportId) -> Option<u64> {
        self.slots
            .iter()
            .find(|s| viewport_id(s.serial) == id)
            .map(|s| s.serial)
    }

    /// Gives every floated surface of `dock` a serial (new surfaces a new one) and forgets the
    /// gone ones. Called at the start of each root pass with whether OS mode is on.
    pub fn sync(&mut self, dock: &DockState<Panel>, active: bool, ctx: &egui::Context) {
        self.active = active;
        self.main_inner = ctx.input(|i| i.viewport().inner_rect);
        if let Some(pos) = ctx.pointer_latest_pos() {
            self.pointer = Some(pos);
        }
        let windows = dock
            .iter_surfaces_indexed()
            .filter(|(_, surface)| matches!(surface, Surface::Window(..)))
            .map(|(index, _)| index);
        let mut kept: Vec<Slot> = Vec::with_capacity(self.slots.len());
        for surface in windows {
            match self.slots.iter().find(|s| s.surface == surface) {
                Some(slot) => kept.push(*slot),
                None => {
                    let serial = self.next_serial;
                    self.next_serial += 1;
                    // A restored layout's window (its rectangle was saved), else where the
                    // pointer was (a torn-off tab), else beside the main window's corner.
                    let first = dock
                        .get_surface(surface)
                        .and_then(|s| s.iter_nodes().find_map(|n| n.tabs()?.first().copied()));
                    let saved = first.and_then(crate::editor_layout::take_restored_rect);
                    let rect = saved.unwrap_or_else(|| {
                        let at = self.pointer.unwrap_or(egui::pos2(64.0, 64.0));
                        (at.x, at.y, DEFAULT_WINDOW_SIZE[0], DEFAULT_WINDOW_SIZE[1])
                    });
                    if saved.is_some() {
                        self.restored.push(serial);
                    }
                    self.rects.insert(serial, rect);
                    kept.push(Slot { surface, serial });
                }
            }
        }
        if kept.len() != self.slots.len() || kept.iter().zip(&self.slots).any(|(a, b)| a != b) {
            let alive: Vec<u64> = kept.iter().map(|s| s.serial).collect();
            self.rects.retain(|serial, _| alive.contains(serial));
            self.restored.retain(|serial| alive.contains(serial));
            self.slots = kept;
        }
    }

    /// Takes the OS-window surfaces out of `dock` (each slot becomes [`Surface::Empty`]) so the main
    /// window's dock area does not draw them. [`Self::unpark`] puts them back.
    pub fn park(&mut self, dock: &mut DockState<Panel>) {
        if self.slots.is_empty() {
            return;
        }
        // egui_dock indexes the focused surface while it draws: an empty (parked) one would panic,
        // so focus moves to the main surface first (its focused group, else none).
        if !dock.focused_leaf().is_some_and(|path| path.surface.is_main()) {
            let node = dock
                .main_surface()
                .focused_leaf()
                .unwrap_or(egui_dock::NodeIndex::root());
            dock.set_focused_node_and_surface(egui_dock::NodePath {
                surface: SurfaceIndex::main(),
                node,
            });
        }
        for slot in &self.slots {
            if let Some(surface) = dock.get_surface_mut(slot.surface) {
                let taken = std::mem::replace(surface, Surface::Empty);
                self.parked.push((slot.surface, taken));
            }
        }
    }

    /// Puts the parked surfaces back. A slot that a new window took meanwhile (a tab torn off in
    /// the main window) moves the parked surface to a free slot.
    pub fn unpark(&mut self, dock: &mut DockState<Panel>) {
        let parked = std::mem::take(&mut self.parked);
        let mut moved = Vec::new();
        // First every slot that is still empty, so a moved surface never takes another's slot.
        for (index, surface) in parked {
            match dock.get_surface_mut(index) {
                Some(slot @ Surface::Empty) => *slot = surface,
                _ => moved.push((index, surface)),
            }
        }
        for (old, surface) in moved {
            let new = dock.add_window(Vec::new());
            if let Some(slot) = dock.get_surface_mut(new) {
                *slot = surface;
            }
            if let Some(s) = self.slots.iter_mut().find(|s| s.surface == old) {
                s.surface = new;
            }
        }
    }

    /// The builder of OS window `serial`: title from its tabs, size and desktop position from its
    /// rectangle (no position when the main window's is unknown).
    #[must_use]
    pub fn builder(&self, serial: u64, dock: &DockState<Panel>) -> egui::ViewportBuilder {
        let titles: Vec<&str> = self
            .surface_of(serial)
            .and_then(|index| dock.get_surface(index))
            .map(|surface| {
                surface
                    .iter_nodes()
                    .filter_map(|n| n.tabs())
                    .flatten()
                    .map(|p| p.title())
                    .collect()
            })
            .unwrap_or_default();
        let (x, y, w, h) = self.rects.get(&serial).copied().unwrap_or((
            64.0,
            64.0,
            DEFAULT_WINDOW_SIZE[0],
            DEFAULT_WINDOW_SIZE[1],
        ));
        let mut builder = egui::ViewportBuilder::default()
            .with_title(format!("{} - Regia Engine Editor", titles.join(", ")))
            .with_app_id("regia.editor-panel")
            .with_inner_size([w.max(120.0), h.max(80.0)]);
        if let Some(main) = self.main_inner {
            builder = builder.with_position(main.min + egui::vec2(x, y));
        }
        // The editor's icon (ADR-20000), so a torn-out panel is not an anonymous window.
        if let Some(icon) = crate::app_icon::egui_icon() {
            builder = builder.with_icon(icon);
        }
        builder
    }

    /// Announces every OS window to egui for this root pass (a deferred viewport the runner draws
    /// with `EditorShell::os_window_ui`; the callback egui keeps is empty).
    pub fn announce(&self, ctx: &egui::Context, dock: &DockState<Panel>) {
        if !self.active {
            return;
        }
        for slot in &self.slots {
            ctx.show_viewport_deferred(
                viewport_id(slot.serial),
                self.builder(slot.serial, dock),
                |_, _| {},
            );
        }
    }

    /// Records where OS window `serial` is (its pass reads its inner rectangle) relative to the
    /// main window, so the layout saves it.
    pub fn observe(&mut self, serial: u64, ctx: &egui::Context) {
        let Some(inner) = ctx.input(|i| i.viewport().inner_rect) else {
            return;
        };
        let origin = self.main_inner.map_or(egui::Pos2::ZERO, |m| m.min);
        let rel = inner.min - origin;
        let rect = (rel.x, rel.y, inner.width(), inner.height());
        if let Some(saved) = self.rects.get_mut(&serial) {
            *saved = rect;
        }
    }

    /// Writes the OS windows' rectangles into `rects` (by surface index), for the layout file.
    pub fn fill_rects(&self, rects: &mut crate::editor_layout::WindowRects) {
        for slot in &self.slots {
            if let Some(rect) = self.rects.get(&slot.serial) {
                rects.insert(slot.surface.0, *rect);
            }
        }
    }

    /// Whether OS window `serial` came from a saved layout (a lost monitor re-docks it).
    #[must_use]
    pub fn restored(&self, serial: u64) -> bool {
        self.restored.contains(&serial)
    }

    /// The window is open where it belongs: it no longer counts as restored.
    pub fn placed(&mut self, serial: u64) {
        self.restored.retain(|s| *s != serial);
    }

    /// Publishes [`OsWindowStatus`] into `world` when the dock, the mode or the support changed
    /// (nothing allocates on an unchanged frame).
    pub fn publish(&mut self, world: &mut World, dock: &DockState<Panel>, supported: bool) {
        let key = crate::editor_layout::dock_fingerprint(dock)
            ^ u64::from(self.active)
            ^ (u64::from(supported) << 1)
            ^ (self.slots.len() as u64).rotate_left(7);
        if self.published == Some(key) && world.contains_resource::<OsWindowStatus>() {
            return;
        }
        self.published = Some(key);
        world.insert_resource(self.status(dock, supported));
    }

    /// The status for agents and menus.
    #[must_use]
    pub fn status(&self, dock: &DockState<Panel>, supported: bool) -> OsWindowStatus {
        let windows = self
            .slots
            .iter()
            .map(|slot| FloatedWindow {
                serial: slot.serial,
                tabs: dock
                    .get_surface(slot.surface)
                    .map(|s| {
                        s.iter_nodes()
                            .filter_map(|n| n.tabs())
                            .flatten()
                            .copied()
                            .collect()
                    })
                    .unwrap_or_default(),
                os_window: self.active,
            })
            .collect();
        OsWindowStatus {
            os_mode: self.active,
            supported,
            windows,
        }
    }
}

/// Floats `panel` in a window of its own at `rect` (main-window points). `Ok(false)` when it
/// already floats.
///
/// # Errors
///
/// `panel_not_open` when the panel has no tab.
pub fn float_panel(dock: &mut DockState<Panel>, panel: Panel, rect: egui::Rect) -> Result<bool, String> {
    let path = dock
        .find_tab(&panel)
        .ok_or_else(|| format!("the {} panel has no tab", panel.id()))?;
    if !path.surface.is_main() {
        return Ok(false);
    }
    dock.detach_tab(path, rect);
    Ok(true)
}

/// Docks `panel` back into the main window's focused group. `Ok(false)` when it is docked.
///
/// # Errors
///
/// `panel_not_open` when the panel has no tab.
pub fn dock_panel(dock: &mut DockState<Panel>, panel: Panel) -> Result<bool, String> {
    let path = dock
        .find_tab(&panel)
        .ok_or_else(|| format!("the {} panel has no tab", panel.id()))?;
    if path.surface.is_main() {
        return Ok(false);
    }
    let Some(tab) = dock.remove_tab(path) else {
        return Ok(false);
    };
    dock_into_main(dock, tab);
    Ok(true)
}

/// Docks every panel of surface `index` back into the main window and removes the surface.
pub fn dock_surface(dock: &mut DockState<Panel>, index: SurfaceIndex) -> usize {
    if index.is_main() {
        return 0;
    }
    let Some(surface) = dock.remove_surface(index) else {
        return 0;
    };
    let tabs: Vec<Panel> = match &surface {
        Surface::Window(tree, _) => tree.iter().filter_map(|n| n.tabs()).flatten().copied().collect(),
        _ => Vec::new(),
    };
    let count = tabs.len();
    for tab in tabs {
        dock_into_main(dock, tab);
    }
    count
}

fn dock_into_main(dock: &mut DockState<Panel>, tab: Panel) {
    let tree = dock.main_surface_mut();
    if tree.focused_leaf().is_some() {
        tree.push_to_focused_leaf(tab);
    } else {
        tree.push_to_first_leaf(tab);
    }
}

// ---------------------------------------------------------------------------------------------
// Drag and drop between windows

/// A drag payload released outside the window it started in, waiting for the pointer to enter
/// another window (the window system keeps the pointer in the source window while a button is
/// held, so the target window sees nothing until the release).
#[derive(Default)]
pub struct DropCarry {
    carried: Option<(egui::DragAndDrop, u64, u64)>,
}

impl DropCarry {
    /// Keeps `ctx`'s drag payload when the pointer was released outside window `source` at frame
    /// `frame`. True when there was a payload to carry.
    pub fn release_outside(&mut self, ctx: &egui::Context, source: u64, frame: u64) -> bool {
        if !egui::DragAndDrop::has_any_payload(ctx) {
            return false;
        }
        let payload = ctx.plugin::<egui::DragAndDrop>().lock().clone();
        self.carried = Some((payload, source, frame));
        true
    }

    /// The pointer moved in window `window` at frame `frame`: when a carried payload waits and
    /// this is another window, the payload is put back into `ctx` and true says the runner must
    /// give the window a release at the pointer.
    pub fn entered(&mut self, ctx: &egui::Context, window: u64, frame: u64) -> bool {
        let Some((_, source, at)) = &self.carried else {
            return false;
        };
        if frame.saturating_sub(*at) > DROP_CARRY_FRAMES {
            self.carried = None;
            return false;
        }
        if *source == window {
            return false;
        }
        let Some((payload, _, _)) = self.carried.take() else {
            return false;
        };
        *ctx.plugin::<egui::DragAndDrop>().lock() = payload;
        true
    }

    /// Whether a payload waits.
    #[must_use]
    pub fn waiting(&self) -> bool {
        self.carried.is_some()
    }
}

// ---------------------------------------------------------------------------------------------
// Tab menu

/// The tab context menu's float items for `tab` (`floating`: the tab is in a floated window).
pub fn tab_menu(ui: &mut egui::Ui, world: &mut World, tab: Panel, floating: bool) {
    if floating {
        // `editor.panel_float { panel, dock: true }`.
        if ui.button("Dock back").clicked() {
            OsWindowRequests::push(world, OsWindowRequest::Dock(tab));
            ui.close();
        }
    } else {
        let label =
            if FloatMode::preferred(world) == FloatMode::Os && world.contains_resource::<OsWindowSupport>() {
                "Float in own window"
            } else {
                "Float"
            };
        // `editor.panel_float { panel }`.
        if ui.button(label).clicked() {
            OsWindowRequests::push(world, OsWindowRequest::Float(tab));
            ui.close();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Agent methods

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn bad(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn status_json(world: &World) -> Value {
    let status = world
        .get_resource::<OsWindowStatus>()
        .cloned()
        .unwrap_or_default();
    let windows: Vec<Value> = status
        .windows
        .iter()
        .map(|w| {
            json!({
                "window": w.serial,
                "tabs": w.tabs.iter().map(|p| p.id()).collect::<Vec<_>>(),
                "os_window": w.os_window,
            })
        })
        .collect();
    json!({
        "mode": FloatMode::preferred(world).id(),
        "os_mode": status.os_mode,
        "supported": status.supported,
        "windows": windows,
        "pending_requests": world.get_resource::<OsWindowRequests>().map_or(0, |r| r.pending.len()),
    })
}

/// Every panel id `doc` names: the main surface's, then the floating windows'.
fn open_panel_ids(doc: &crate::editor_layout::LayoutDoc) -> Vec<&str> {
    let mut ids = doc.panel_ids();
    ids.extend(
        doc.floating
            .iter()
            .flat_map(|w| w.tabs.iter().map(String::as_str)),
    );
    ids
}

/// `editor.panel_float { panel, os_window?, dock? }`.
fn panel_float(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = params.get("panel").and_then(Value::as_str).unwrap_or_default();
    let panel = Panel::from_id(id).ok_or_else(|| {
        bad(
            "unknown_panel",
            format!("no panel has the id {id:?}"),
            "ids such as inspector, content, material_graph",
        )
    })?;
    let open = world
        .get_resource::<crate::editor_layout::LayoutSnapshot>()
        .is_none_or(|s| open_panel_ids(&s.doc).contains(&panel.id()));
    if !open {
        return Err(bad(
            "panel_not_open",
            format!("the {} panel has no tab", panel.id()),
            "open it first (editor.panel_replace)",
        ));
    }
    let dock = params.get("dock").and_then(Value::as_bool).unwrap_or(false);
    let os_now = world
        .get_resource::<OsWindowStatus>()
        .is_some_and(|s| s.os_mode || (s.supported && FloatMode::preferred(world) == FloatMode::Os));
    if !dock
        && let Some(wanted) = params.get("os_window").and_then(Value::as_bool)
        && wanted != os_now
    {
        return Err(bad(
            "float_mode_mismatch",
            if wanted {
                "floated panels are windows inside the main window here"
            } else {
                "floated panels are OS windows here"
            },
            if wanted {
                "set ui.floating_windows to os (editor.prefs_set) in the editor binary; a headless editor has no OS windows"
            } else {
                "set ui.floating_windows to in_window (editor.prefs_set)"
            },
        ));
    }
    let request = if dock {
        OsWindowRequest::Dock(panel)
    } else {
        OsWindowRequest::Float(panel)
    };
    OsWindowRequests::push(world, request);
    let mut out = status_json(world);
    out["queued"] = json!({ "panel": panel.id(), "action": if dock { "dock" } else { "float" } });
    out["os_window"] = json!(!dock && os_now);
    Ok(out)
}

/// `editor.panel_instances`.
fn panel_instances(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let status = world
        .get_resource::<OsWindowStatus>()
        .cloned()
        .unwrap_or_default();
    let open: Vec<&'static str> = world
        .get_resource::<crate::editor_layout::LayoutSnapshot>()
        .map(|s| {
            open_panel_ids(&s.doc)
                .iter()
                .filter_map(|id| Panel::from_id(id).map(Panel::id))
                .collect()
        })
        .unwrap_or_default();
    let panels: Vec<Value> = open
        .iter()
        .filter_map(|id| Panel::from_id(id))
        .map(|panel| {
            let window = status.window_of(panel);
            json!({
                "panel": panel.id(),
                "instance": panel.instance(),
                "floating": window.is_some(),
                "os_window": window.is_some_and(|w| w.os_window),
                "window": window.map(|w| w.serial),
            })
        })
        .collect();
    let mut out = status_json(world);
    out["panels"] = json!(panels);
    Ok(out)
}

/// This module's agent methods, installed by `agent_ext::all_methods`.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.panel_float",
                tool: "panel_float",
                description: "Float a panel in a window of its own on the next frame (an OS window when ui.floating_windows is os and the editor runs with a window system; else a window inside the main one), or dock it back with dock.",
                params: &[
                    ParamSchema {
                        name: "panel",
                        kind: "string",
                        required: true,
                        description: "The panel id (inspector, content, material_graph, ...).",
                    },
                    ParamSchema {
                        name: "os_window",
                        kind: "boolean",
                        required: false,
                        description: "Require (true) or refuse (false) an OS window: float_mode_mismatch when the mode differs.",
                    },
                    ParamSchema {
                        name: "dock",
                        kind: "boolean",
                        required: false,
                        description: "Dock the panel back into the main window instead.",
                    },
                ],
                result: "{ mode, os_mode, supported, windows: [{ window, tabs, os_window }], pending_requests, queued, os_window }",
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_float as Handler,
        ),
        (
            MethodSchema {
                name: "editor.panel_instances",
                tool: "panel_instances",
                description: "The open panels with their instance (0 for a panel's first tab; outliner#1 is instance 1, P-003), whether each floats and whether it is in its own OS window; and the floated windows.",
                params: &[],
                result: "{ mode, os_mode, supported, windows, pending_requests, panels: [{ panel, instance, floating, os_window, window }] }",
                mutating: false,
                permission: Permission::Read,
            },
            panel_instances as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_window_whose_monitor_is_gone_is_not_visible() {
        let monitors = [[0, 0, 1920, 1080], [1920, 0, 2560, 1440]];
        // On the second monitor.
        assert!(visible_on_monitors([2000, 100], [480, 360], &monitors));
        // Straddling the edge with enough on the first.
        assert!(visible_on_monitors([1900, 100], [480, 360], &monitors));
        // The second monitor unplugged: the same window is on no monitor.
        assert!(!visible_on_monitors([2000, 100], [480, 360], &monitors[..1]));
        // Only a sliver on screen.
        assert!(!visible_on_monitors([1900, 1070], [480, 360], &monitors[..1]));
        // No monitor known (the window system gives none): trust the window.
        assert!(visible_on_monitors([-9000, -9000], [480, 360], &[]));
        assert!(outside_window([-1.0, 10.0], [100, 100]));
        assert!(outside_window([100.0, 10.0], [100, 100]));
        assert!(!outside_window([99.0, 99.0], [100, 100]));
    }

    #[test]
    fn float_and_dock_move_a_panel_and_never_duplicate_it() {
        let mut dock = crate::editor_layout::default_dock();
        let before = dock.iter_all_tabs().count();
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(400.0, 300.0));
        assert_eq!(float_panel(&mut dock, Panel::Inspector, rect), Ok(true));
        let path = dock.find_tab(&Panel::Inspector).unwrap();
        assert!(!path.surface.is_main());
        assert_eq!(float_panel(&mut dock, Panel::Inspector, rect), Ok(false));
        assert_eq!(dock.iter_all_tabs().count(), before);
        assert_eq!(dock_panel(&mut dock, Panel::Inspector), Ok(true));
        assert!(dock.find_tab(&Panel::Inspector).unwrap().surface.is_main());
        assert_eq!(dock_panel(&mut dock, Panel::Inspector), Ok(false));
        assert!(float_panel(&mut dock, Panel::Preferences, rect).is_err());
        // A window of two docks both back and goes away.
        float_panel(&mut dock, Panel::Content, rect).unwrap();
        let index = dock.find_tab(&Panel::Content).unwrap().surface;
        let target = dock.find_tab(&Panel::Content).unwrap().node_path();
        let outliner = dock.find_tab(&Panel::Outliner).unwrap();
        dock.move_tab(
            outliner,
            egui_dock::TabDestination::Node(target, egui_dock::TabInsert::Append),
        );
        assert_eq!(dock_surface(&mut dock, index), 2);
        assert!(
            dock.get_surface(index)
                .is_none_or(|s| matches!(s, Surface::Empty))
        );
        assert_eq!(dock.iter_all_tabs().count(), before);
    }

    #[test]
    fn parking_hides_os_surfaces_and_unpark_survives_a_new_window_in_the_slot() {
        let ctx = egui::Context::default();
        let mut dock = crate::editor_layout::default_dock();
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(400.0, 300.0));
        float_panel(&mut dock, Panel::Inspector, rect).unwrap();
        let mut os = OsWindows::default();
        os.sync(&dock, true, &ctx);
        assert_eq!(os.serials(), vec![0]);
        os.park(&mut dock);
        assert!(dock.find_tab(&Panel::Inspector).is_none(), "parked");
        // A tab torn off while parked takes the empty slot.
        float_panel(&mut dock, Panel::Content, rect).unwrap();
        os.unpark(&mut dock);
        let inspector = dock.find_tab(&Panel::Inspector).unwrap().surface;
        let content = dock.find_tab(&Panel::Content).unwrap().surface;
        assert_ne!(inspector, content);
        assert_eq!(os.surface_of(0), Some(inspector));
        os.sync(&dock, true, &ctx);
        assert_eq!(os.serials().len(), 2, "the new window gets its own serial");
        assert_eq!(os.surface_of(0), Some(inspector), "the old one keeps its serial");
        // The builder names the window after its tabs.
        let builder = os.builder(0, &dock);
        assert!(
            builder
                .title
                .as_deref()
                .unwrap_or_default()
                .starts_with("Inspector")
        );
        assert_ne!(viewport_id(0), viewport_id(1));
        assert_eq!(os.serial_of_viewport(viewport_id(0)), Some(0));
    }

    #[test]
    fn a_carried_drop_reaches_only_another_window_and_expires() {
        let ctx = egui::Context::default();
        let mut carry = DropCarry::default();
        assert!(
            !carry.release_outside(&ctx, 1, 10),
            "no payload, nothing to carry"
        );
        egui::DragAndDrop::set_payload(&ctx, 42_u32);
        assert!(carry.release_outside(&ctx, 1, 10));
        // The source window's pass ends the drag.
        egui::DragAndDrop::clear_payload(&ctx);
        assert!(!carry.entered(&ctx, 1, 11), "the source window does not claim it");
        assert!(carry.entered(&ctx, 2, 12));
        assert_eq!(egui::DragAndDrop::payload::<u32>(&ctx).as_deref(), Some(&42));
        assert!(!carry.waiting());
        // Expired.
        assert!(carry.release_outside(&ctx, 1, 100));
        egui::DragAndDrop::clear_payload(&ctx);
        assert!(!carry.entered(&ctx, 2, 100 + DROP_CARRY_FRAMES + 1));
        assert!(!egui::DragAndDrop::has_any_payload(&ctx));
        assert!(!carry.waiting());
    }

    #[test]
    fn the_preference_picks_the_mode() {
        assert_eq!(FloatMode::from_id("os"), Some(FloatMode::Os));
        assert_eq!(FloatMode::from_id("in_window"), Some(FloatMode::InWindow));
        assert_eq!(FloatMode::from_id("tabs"), None);
        for id in FLOAT_MODE_CHOICES {
            assert_eq!(FloatMode::from_id(id).map(FloatMode::id), Some(*id));
        }
        let world = World::new();
        assert_eq!(FloatMode::preferred(&world).id(), DEFAULT_FLOAT_MODE);
        assert!(crate::editor_prefs::find_pref(FLOATING_WINDOWS_PREF).is_some());
    }
}
