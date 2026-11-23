// SPDX-License-Identifier: MPL-2.0
//! Menus (ADR-273 lane 5, gap W03): [`PopupMenu`], [`ContextMenu`] and [`MenuBar`], drawn on the
//! popup layer ([`crate::popup`]).
//!
//! A menu is a list of [`MenuEntry`]s: items (with an optional check mark, a shortcut text slot
//! and a submenu) and separators. The list lives on the node that hosts the menu, so the host is
//! the one place game code and agents read and change it; an open menu is a popup whose rows
//! point back into the host's list ([`MenuPopup`], [`MenuRow`]).
//!
//! - **Pointer:** a click on a [`PopupMenu`] host or a [`MenuBar`] title opens its menu, a
//!   secondary click (or a long press on a [`crate::LongPressable`] host) opens a
//!   [`ContextMenu`] at the pointer. Hovering an item with a submenu opens it; hovering another
//!   title while a menu bar menu is open switches to that title's menu.
//! - **Keyboard and gamepad:** Up/Down move through the items (the focus system), Enter or A
//!   activates, Right or A on an item with a submenu opens it, Left or B closes a submenu, and in
//!   a menu bar menu Left/Right move to the neighbouring menu. The `ContextMenu` key, Shift+F10 or
//!   gamepad Y open the context menu of the focused node.
//! - **Search:** a menu with more than [`MENU_SEARCH_THRESHOLD`] entries (or [`MenuSearch::Always`])
//!   starts with a search field that hides the items whose label does not contain the text; Enter
//!   in the field activates the first item left.
//!
//! Activating an item flips a checkable item's `checked`, records a [`MenuActivation`] in
//! [`MenuActivations`], sends `UiEventKind::ValueChanged(UiValue::Text(path))` on the host (the
//! item ids joined with `/`) and closes the menu. Looks come from the theme types `Menu`,
//! `MenuBar`, `MenuItem`, `MenuItem.checked`, `MenuItem.shortcut` and `Separator`.
//!
//! Agent methods: `ui.open_popup`, `ui.close_popup`, `ui.menu`, `ui.menu_activate`
//! (`book/agents/ui-menus.md`).

use regia_app::prelude::*;
use regia_ecs::ecs::message::MessageCursor;
use regia_platform::{ButtonState, GamepadAxis, GamepadAxisChanged, GamepadButton, GamepadButtonChanged};
use serde_json::{Map, Value, json};

use crate::agent_bridge::UiBridgeError;
use crate::focus::{DefaultFocus, FocusState, Focusable, GamepadNavConfig, NavDirection, stick_direction};
use crate::interaction::{UiEvent, UiEventKind, UiPointer, UiValue};
use crate::node::{Classes, ComputedNode, Disabled, Interactable, NodeStyle, UiNode};
use crate::popup::{
    Popup, PopupHandle, PopupKind, PopupPlacement, PopupRequest, PopupStack, close_popup, open_popup,
};
use crate::style::{Align, Dimension, Display, Edges, FlexDirection, Style};
use crate::text::{Text, TextContent, TextSpan, WrapMode};
use crate::theme::Themed;
use crate::widget_style::{UiDensity, WidgetSize};
use crate::widgets::{Dropdown, TextInput};

/// Menus with more entries than this show a search field ([`MenuSearch::Auto`]).
pub const MENU_SEARCH_THRESHOLD: usize = 12;

/// The check mark drawn before a checked item.
const CHECK_GLYPH: &str = "\u{2713}";
/// The arrow drawn after an item with a submenu.
const SUBMENU_GLYPH: &str = "\u{203a}";

/// One entry of a menu.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuEntry {
    /// An item.
    Item(MenuItem),
    /// A separator line.
    Separator,
}

impl MenuEntry {
    /// The item, unless this is a separator.
    pub fn item(&self) -> Option<&MenuItem> {
        match self {
            Self::Item(item) => Some(item),
            Self::Separator => None,
        }
    }
}

impl From<MenuItem> for MenuEntry {
    fn from(item: MenuItem) -> Self {
        Self::Item(item)
    }
}

/// A menu item.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    /// Stable id: the path segment agents and [`MenuActivation::path`] use. Empty uses the label.
    pub id: String,
    /// Shown text.
    pub label: String,
    /// Disabled items are shown dimmed and cannot be activated.
    pub enabled: bool,
    /// Activation flips `checked`.
    pub checkable: bool,
    /// The check state of a checkable item.
    pub checked: bool,
    /// Shortcut text shown right-aligned. When empty, the row shows the chord the shortcut
    /// registry binds to the item's key (`UiShortcuts::chord_label` of [`MenuItem::key`]).
    pub shortcut: String,
    /// Entries of the submenu; an item with a submenu opens it instead of activating.
    pub submenu: Vec<MenuEntry>,
}

impl Default for MenuItem {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            enabled: true,
            checkable: false,
            checked: false,
            shortcut: String::new(),
            submenu: Vec::new(),
        }
    }
}

impl MenuItem {
    /// An enabled item.
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            ..Default::default()
        }
    }

    /// Makes the item checkable with an initial state.
    #[must_use]
    pub fn checkable(mut self, checked: bool) -> Self {
        self.checkable = true;
        self.checked = checked;
        self
    }

    /// Disables the item.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// Sets the shortcut text.
    #[must_use]
    pub fn with_shortcut(mut self, text: impl Into<String>) -> Self {
        self.shortcut = text.into();
        self
    }

    /// Gives the item a submenu.
    #[must_use]
    pub fn with_submenu(mut self, entries: Vec<MenuEntry>) -> Self {
        self.submenu = entries;
        self
    }

    /// The path segment: `id`, or `label` when `id` is empty.
    pub fn key(&self) -> &str {
        if self.id.is_empty() { &self.label } else { &self.id }
    }
}

/// When a menu shows its search field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MenuSearch {
    /// When the menu has more than [`MENU_SEARCH_THRESHOLD`] entries.
    #[default]
    Auto,
    /// Always.
    Always,
    /// Never.
    Never,
}

/// A menu opened by clicking (or activating) the node that carries it.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct PopupMenu {
    /// The entries.
    pub entries: Vec<MenuEntry>,
    /// Which side of the node the menu opens on.
    pub placement: PopupPlacement,
    /// Search field rule.
    pub search: MenuSearch,
}

/// A menu opened at the pointer by a secondary click (or a long press) on the node, or below
/// the focused node by the `ContextMenu` key, Shift+F10 or gamepad Y.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct ContextMenu {
    /// The entries.
    pub entries: Vec<MenuEntry>,
    /// Search field rule.
    pub search: MenuSearch,
}

/// A row of menu titles; each title opens its item's submenu below it.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct MenuBar {
    /// The menus: each item's label is the title, its submenu the menu.
    pub menus: Vec<MenuItem>,
}

/// Which list of a host an open menu shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuSource {
    /// The host's [`PopupMenu`].
    Popup,
    /// The host's [`ContextMenu`].
    Context,
    /// Menu `n` of the host's [`MenuBar`].
    Bar(usize),
}

impl MenuSource {
    /// The protocol name of the host kind.
    pub fn kind_name(self) -> &'static str {
        match self {
            Self::Popup => "popup_menu",
            Self::Context => "context_menu",
            Self::Bar(_) => "menu_bar",
        }
    }
}

/// On an open menu's popup root: whose list it shows.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct MenuPopup {
    /// The node that hosts the menu.
    pub host: Entity,
    /// Which of the host's lists.
    pub source: MenuSource,
    /// Item indices from the list's top to this submenu (empty for the top level).
    pub path: Vec<usize>,
}

/// On a menu row: the popup it belongs to and its entry index.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuRow {
    /// The menu popup root.
    pub popup: Entity,
    /// Index into the popup's entry list.
    pub index: usize,
}

/// On a menu bar title: the menu index.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuBarTitle(pub usize);

/// Marks the check mark text of a row.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
struct MenuCheckGlyph;

/// Marks the shortcut text of a row (a slot filled by the shortcut registry).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuShortcutSlot;

/// The search field of an open menu.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct MenuSearchField {
    /// The menu popup root.
    pub popup: Entity,
    last: String,
}

/// One activated menu item.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuActivation {
    /// The node that hosts the menu.
    pub host: Entity,
    /// Item keys from the top of the list (a menu bar's path starts with the menu's key).
    pub path: Vec<String>,
    /// The new check state of a checkable item.
    pub checked: Option<bool>,
}

/// Menu items activated recently. An entry is dropped at the start of the second `ui_widgets`
/// run after it was recorded, so every system sees it during at least one full frame (and may see
/// it in two; read the host's `ValueChanged` message to see each activation exactly once).
#[derive(Resource, Debug, Default)]
pub struct MenuActivations {
    /// The activations, oldest first.
    pub activations: Vec<MenuActivation>,
    stale: usize,
}

/// One frame's menu-relevant input, read once per `ui_widgets` run.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct FrameInput {
    /// Right arrow, d-pad right or the left stick pushed right.
    pub right: bool,
    /// Left arrow, d-pad left or the left stick pushed left.
    pub left: bool,
    /// Printable characters typed (type-ahead).
    pub chars: Vec<char>,
    /// The `ContextMenu` key, Shift+F10 or gamepad Y.
    pub context: bool,
}

/// Input cursors and stick state between frames.
#[derive(Resource, Default)]
struct MenuInputState {
    keys: MessageCursor<regia_platform::KeyboardInput>,
    buttons: MessageCursor<GamepadButtonChanged>,
    axes: MessageCursor<GamepadAxisChanged>,
    stick: [f32; 2],
    stick_dir: Option<NavDirection>,
    dpad_x: i8,
    last_cursor: Option<[f32; 2]>,
}

/// Reads this frame's keyboard and gamepad messages (all unread ones, with private cursors).
pub(crate) fn read_frame_input(world: &mut World) -> FrameInput {
    let mut state = world.remove_resource::<MenuInputState>().unwrap_or_default();
    let mut out = FrameInput::default();
    let shift = world
        .get_resource::<UiPointer>()
        .is_some_and(|p| p.modifiers.shift);
    if let Some(keys) = world.get_resource::<Messages<regia_platform::KeyboardInput>>() {
        for e in state.keys.read(keys) {
            if e.state != ButtonState::Pressed {
                continue;
            }
            match e.logical_key.to_ascii_lowercase().as_str() {
                "arrowright" | "right" => out.right = true,
                "arrowleft" | "left" => out.left = true,
                "contextmenu" => out.context = true,
                "f10" if shift => out.context = true,
                _ => {
                    let mut chars = e.logical_key.chars();
                    if let (Some(c), None) = (chars.next(), chars.next())
                        && !c.is_control()
                        && !c.is_whitespace()
                    {
                        out.chars.push(c);
                    }
                }
            }
        }
    }
    if let Some(buttons) = world.get_resource::<Messages<GamepadButtonChanged>>() {
        for e in state.buttons.read(buttons) {
            if e.state != ButtonState::Pressed {
                continue;
            }
            match e.button {
                GamepadButton::DPadRight => out.right = true,
                GamepadButton::DPadLeft => out.left = true,
                GamepadButton::North => out.context = true,
                _ => {}
            }
        }
    }
    if let Some(axes) = world.get_resource::<Messages<GamepadAxisChanged>>() {
        for e in state.axes.read(axes) {
            match e.axis {
                GamepadAxis::LeftStickX => state.stick[0] = e.value,
                GamepadAxis::LeftStickY => state.stick[1] = e.value,
                GamepadAxis::DPadX => {
                    let dir = if e.value > 0.5 {
                        1
                    } else if e.value < -0.5 {
                        -1
                    } else {
                        0
                    };
                    if dir != state.dpad_x {
                        out.right |= dir == 1;
                        out.left |= dir == -1;
                    }
                    state.dpad_x = dir;
                }
                _ => {}
            }
        }
    }
    let deadzone = world
        .get_resource::<GamepadNavConfig>()
        .map_or(0.55, |c| c.deadzone);
    let dir = stick_direction(state.stick[0], state.stick[1], deadzone);
    if dir != state.stick_dir {
        out.right |= dir == Some(NavDirection::Right);
        out.left |= dir == Some(NavDirection::Left);
        state.stick_dir = dir;
    }
    world.insert_resource(state);
    out
}

/// The index of the option type-ahead selects: `buffer` is what was typed since the last pause.
/// A buffer of one repeated letter cycles through the options starting with it, after `current`;
/// a longer buffer selects the first option (from `current` on, wrapping) whose label starts with
/// it. Case-insensitive. `None` when nothing matches.
pub fn type_ahead(options: &[String], current: Option<usize>, buffer: &str) -> Option<usize> {
    let wanted = buffer.to_lowercase();
    let first = wanted.chars().next()?;
    let n = options.len();
    if n == 0 {
        return None;
    }
    let starts = |i: usize, prefix: &str| options[i].to_lowercase().starts_with(prefix);
    let order = |from: usize| (0..n).map(move |k| (from + k) % n);
    let repeated = wanted.chars().all(|c| c == first);
    let single = first.to_string();
    if wanted.chars().count() > 1 {
        // A real prefix: keep the current option while it still matches.
        let from = current.unwrap_or(0).min(n - 1);
        if let Some(i) = order(from).find(|&i| starts(i, &wanted)) {
            return Some(i);
        }
        if !repeated {
            return None;
        }
    }
    // `c % n` first: `Some(usize::MAX)` overflowed `c + 1`.
    // Regression: tests/tqs_sweep.rs::type_ahead_survives_a_current_index_at_usize_max
    let from = current.map_or(0, |c| (c % n + 1) % n);
    order(from).find(|&i| starts(i, &single))
}

/// The entry list `source` names on `host`.
fn root_entries(world: &World, host: Entity, source: MenuSource) -> Option<&[MenuEntry]> {
    match source {
        MenuSource::Popup => world.get::<PopupMenu>(host).map(|m| m.entries.as_slice()),
        MenuSource::Context => world.get::<ContextMenu>(host).map(|m| m.entries.as_slice()),
        MenuSource::Bar(i) => world
            .get::<MenuBar>(host)
            .and_then(|b| b.menus.get(i))
            .map(|m| m.submenu.as_slice()),
    }
}

/// The entry list at `path` (item indices) below the list `source` names.
pub fn entries_at<'w>(
    world: &'w World,
    host: Entity,
    source: MenuSource,
    path: &[usize],
) -> Option<&'w [MenuEntry]> {
    let mut list = root_entries(world, host, source)?;
    for &i in path {
        list = list.get(i)?.item()?.submenu.as_slice();
    }
    Some(list)
}

/// The item at `path` (its last index names the item).
pub fn item_at<'w>(
    world: &'w World,
    host: Entity,
    source: MenuSource,
    path: &[usize],
) -> Option<&'w MenuItem> {
    let (last, parents) = path.split_last()?;
    entries_at(world, host, source, parents)?.get(*last)?.item()
}

fn item_in_mut<'a>(entries: &'a mut [MenuEntry], path: &[usize]) -> Option<&'a mut MenuItem> {
    let (first, rest) = path.split_first()?;
    match entries.get_mut(*first)? {
        MenuEntry::Item(item) => {
            if rest.is_empty() {
                Some(item)
            } else {
                item_in_mut(&mut item.submenu, rest)
            }
        }
        MenuEntry::Separator => None,
    }
}

/// Runs `f` on the item at `path`; `None` when there is none.
fn with_item_mut<R>(
    world: &mut World,
    host: Entity,
    source: MenuSource,
    path: &[usize],
    f: impl FnOnce(&mut MenuItem) -> R,
) -> Option<R> {
    match source {
        MenuSource::Popup => world
            .get_mut::<PopupMenu>(host)
            .and_then(|mut m| item_in_mut(&mut m.entries, path).map(f)),
        MenuSource::Context => world
            .get_mut::<ContextMenu>(host)
            .and_then(|mut m| item_in_mut(&mut m.entries, path).map(f)),
        MenuSource::Bar(i) => world.get_mut::<MenuBar>(host).and_then(|mut b| {
            let menu = b.menus.get_mut(i)?;
            item_in_mut(&mut menu.submenu, path).map(f)
        }),
    }
}

/// The keys of the items along `path` (a menu bar's path starts with the menu's key).
fn path_keys(world: &World, host: Entity, source: MenuSource, path: &[usize]) -> Vec<String> {
    let mut keys = Vec::with_capacity(path.len() + 1);
    if let MenuSource::Bar(i) = source
        && let Some(menu) = world.get::<MenuBar>(host).and_then(|b| b.menus.get(i))
    {
        keys.push(menu.key().to_owned());
    }
    let Some(mut list) = root_entries(world, host, source) else {
        return keys;
    };
    for &i in path {
        let Some(item) = list.get(i).and_then(MenuEntry::item) else {
            break;
        };
        keys.push(item.key().to_owned());
        list = item.submenu.as_slice();
    }
    keys
}

/// Why a menu item cannot be activated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuError {
    /// No item at the path.
    NotFound,
    /// The item is disabled.
    Disabled,
    /// The item opens a submenu.
    HasSubmenu,
}

/// Activates the item at `path` of the list `source` names on `host`, exactly as a click does:
/// flips a checkable item, records a [`MenuActivation`] and sends the host's `ValueChanged`.
/// Does not close any popup.
pub fn activate_item(
    world: &mut World,
    host: Entity,
    source: MenuSource,
    path: &[usize],
) -> Result<MenuActivation, MenuError> {
    let item = item_at(world, host, source, path).ok_or(MenuError::NotFound)?;
    if !item.enabled {
        return Err(MenuError::Disabled);
    }
    if !item.submenu.is_empty() {
        return Err(MenuError::HasSubmenu);
    }
    let checked = with_item_mut(world, host, source, path, |item| {
        if item.checkable {
            item.checked = !item.checked;
            Some(item.checked)
        } else {
            None
        }
    })
    .flatten();
    let activation = MenuActivation {
        host,
        path: path_keys(world, host, source, path),
        checked,
    };
    world.init_resource::<MenuActivations>();
    world
        .resource_mut::<MenuActivations>()
        .activations
        .push(activation.clone());
    world.write_message(UiEvent::new(
        host,
        UiEventKind::ValueChanged(UiValue::Text(activation.path.join("/"))),
    ));
    refresh_open_rows(world);
    Ok(activation)
}

/// Where a menu opens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MenuAnchor {
    /// Next to a node, on a side.
    Node(Entity, PopupPlacement),
    /// At a point (physical px): below and right of it.
    Point([f32; 2]),
}

fn search_wanted(world: &World, host: Entity, source: MenuSource, count: usize) -> bool {
    let rule = match source {
        MenuSource::Popup => world.get::<PopupMenu>(host).map(|m| m.search),
        MenuSource::Context => world.get::<ContextMenu>(host).map(|m| m.search),
        MenuSource::Bar(_) => None,
    }
    .unwrap_or_default();
    match rule {
        MenuSearch::Always => true,
        MenuSearch::Never => false,
        MenuSearch::Auto => count > MENU_SEARCH_THRESHOLD,
    }
}

fn row_classes(item: &MenuItem) -> Vec<String> {
    let mut classes = vec!["MenuItem".to_owned()];
    if item.checkable && item.checked {
        classes.push("MenuItem.checked".to_owned());
    }
    classes
}

/// The shortcut text a menu row shows: the item's own `shortcut`, or, when that slot is empty, the
/// chord the shortcut registry binds to the item's key (`UiShortcuts::chord_label`, ADR-374,
/// ADR-5665). Empty when neither has one.
fn shortcut_text(world: &World, item: &MenuItem) -> String {
    if !item.shortcut.is_empty() {
        return item.shortcut.clone();
    }
    world
        .get_resource::<crate::shortcuts::UiShortcuts>()
        .and_then(|shortcuts| shortcuts.chord_label(item.key()))
        .unwrap_or_default()
}

fn label_node(world: &mut World, parent: Entity, text: &str, grow: bool) -> Entity {
    world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                flex_grow: if grow { 1.0 } else { 0.0 },
                ..Default::default()
            }),
            Text {
                spans: vec![TextSpan::lit(text.to_owned())],
                wrap: WrapMode::None,
                ..Default::default()
            },
            ChildOf(parent),
        ))
        .id()
}

/// A row style: a `size.control.sm` tall flex row with `size.pad-x.sm` inline padding.
pub(crate) fn row_style(world: &World) -> Style {
    let density = world.get_resource::<UiDensity>().copied().unwrap_or_default();
    let pad = density.pad_x(WidgetSize::Sm);
    Style {
        direction: FlexDirection::Row,
        align_items: Align::Center,
        gap: [Dimension::Points(8.0), Dimension::Points(8.0)],
        padding: Edges {
            left: Dimension::Points(pad),
            top: Dimension::Points(0.0),
            right: Dimension::Points(pad),
            bottom: Dimension::Points(0.0),
        },
        min_size: [
            Dimension::Auto,
            Dimension::Points(density.control_height(WidgetSize::Sm)),
        ],
        ..Default::default()
    }
}

fn spawn_row(world: &mut World, panel: Entity, popup: Entity, index: usize, item: &MenuItem) -> Entity {
    let style = row_style(world);
    let row = world
        .spawn((
            UiNode,
            NodeStyle::new(style),
            Interactable::default(),
            Focusable::default(),
            WidgetSize::Sm,
            MenuRow { popup, index },
            Classes(row_classes(item)),
            Themed::default(),
            ChildOf(panel),
        ))
        .id();
    if !item.enabled {
        world.entity_mut(row).insert(Disabled);
    }
    let check = label_node(
        world,
        row,
        if item.checkable && item.checked {
            CHECK_GLYPH
        } else {
            ""
        },
        false,
    );
    if let Some(mut style) = world.get_mut::<NodeStyle>(check) {
        style.0.min_size = [Dimension::Points(14.0), Dimension::Auto];
    }
    world.entity_mut(check).insert(MenuCheckGlyph);
    label_node(world, row, &item.label, true);
    let text = shortcut_text(world, item);
    let shortcut = label_node(world, row, &text, false);
    world.entity_mut(shortcut).insert((
        MenuShortcutSlot,
        Classes(vec!["MenuItem.shortcut".to_owned()]),
        Themed::default(),
    ));
    if !item.submenu.is_empty() {
        label_node(world, row, SUBMENU_GLYPH, false);
    }
    row
}

/// Opens the list at `path` of `source` on `host` as a menu popup. `parent` is the menu a
/// submenu belongs to; focus returns to `opener` when it closes. `None` when the list is empty or
/// missing.
pub fn open_menu(
    world: &mut World,
    host: Entity,
    source: MenuSource,
    path: Vec<usize>,
    anchor: MenuAnchor,
    parent: Option<Entity>,
    opener: Option<Entity>,
) -> Option<PopupHandle> {
    let entries: Vec<MenuEntry> = entries_at(world, host, source, &path)?.to_vec();
    if entries.is_empty() {
        return None;
    }
    let (anchor_node, point, placement) = match anchor {
        MenuAnchor::Node(node, placement) => (Some(node), None, placement),
        MenuAnchor::Point(p) => (None, Some(p), PopupPlacement::Below),
    };
    let handle = open_popup(
        world,
        PopupRequest {
            kind: PopupKind::Menu,
            anchor: anchor_node,
            point,
            placement,
            opener,
            parent,
            interactive: true,
            min_width: 160.0,
            classes: vec!["Menu".to_owned()],
        },
    );
    world
        .entity_mut(handle.root)
        .insert(MenuPopup { host, source, path });
    let search = search_wanted(world, host, source, entries.len());
    if search {
        let mut style = row_style(world);
        style.margin.bottom = Dimension::Points(4.0);
        world.spawn((
            UiNode,
            NodeStyle::new(style),
            Interactable::default(),
            Focusable::default(),
            DefaultFocus,
            TextInput {
                placeholder: "Search".to_owned(),
                ..Default::default()
            },
            MenuSearchField {
                popup: handle.root,
                last: String::new(),
            },
            ChildOf(handle.panel),
        ));
    }
    let mut default_set = search;
    for (index, entry) in entries.iter().enumerate() {
        match entry {
            MenuEntry::Separator => {
                world.spawn((
                    UiNode,
                    NodeStyle::new(Style {
                        size: [Dimension::Auto, Dimension::Points(1.0)],
                        margin: Edges {
                            left: Dimension::Points(0.0),
                            top: Dimension::Points(4.0),
                            right: Dimension::Points(0.0),
                            bottom: Dimension::Points(4.0),
                        },
                        ..Default::default()
                    }),
                    MenuRow {
                        popup: handle.root,
                        index,
                    },
                    Classes(vec!["Separator".to_owned()]),
                    Themed::default(),
                    ChildOf(handle.panel),
                ));
            }
            MenuEntry::Item(item) => {
                let row = spawn_row(world, handle.panel, handle.root, index, item);
                if !default_set && item.enabled {
                    world.entity_mut(row).insert(DefaultFocus);
                    default_set = true;
                }
            }
        }
    }
    Some(handle)
}

/// The open menu popup showing `path` of `source` on `host`, if any.
pub fn open_menu_of(world: &World, host: Entity, source: MenuSource, path: &[usize]) -> Option<Entity> {
    world
        .get_resource::<PopupStack>()?
        .popups()
        .iter()
        .copied()
        .find(|p| {
            world
                .get::<MenuPopup>(*p)
                .is_some_and(|m| m.host == host && m.source == source && m.path == path)
        })
}

/// The open top-level menus of `host` (any source).
fn open_menus_of_host(world: &World, host: Entity) -> Vec<Entity> {
    world
        .get_resource::<PopupStack>()
        .map(|s| {
            s.popups()
                .iter()
                .copied()
                .filter(|p| {
                    world
                        .get::<MenuPopup>(*p)
                        .is_some_and(|m| m.host == host && m.path.is_empty())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The outermost menu popup of the chain `popup` belongs to.
fn chain_root(world: &World, popup: Entity) -> Entity {
    let mut current = popup;
    for _ in 0..64 {
        match world.get::<Popup>(current).and_then(|p| p.parent) {
            Some(parent) if world.get::<MenuPopup>(parent).is_some() => current = parent,
            _ => break,
        }
    }
    current
}

/// Closes the open popups that belong to `popup` (its submenus), keeping `popup` itself.
fn close_children(world: &mut World, popup: Entity) {
    let children: Vec<Entity> = world
        .get_resource::<PopupStack>()
        .map(|s| {
            s.popups()
                .iter()
                .copied()
                .filter(|p| world.get::<Popup>(*p).is_some_and(|c| c.parent == Some(popup)))
                .collect()
        })
        .unwrap_or_default();
    for child in children {
        close_popup(world, child);
    }
}

/// Opens the submenu of `row` (closing any other submenu of the same menu). Returns the new
/// popup, or the one already open for this row.
fn open_submenu(world: &mut World, row_entity: Entity) -> Option<Entity> {
    let row = *world.get::<MenuRow>(row_entity)?;
    let menu = world.get::<MenuPopup>(row.popup)?.clone();
    let mut path = menu.path.clone();
    path.push(row.index);
    let item = item_at(world, menu.host, menu.source, &path)?;
    if !item.enabled || item.submenu.is_empty() {
        return None;
    }
    if let Some(open) = open_menu_of(world, menu.host, menu.source, &path) {
        return Some(open);
    }
    close_children(world, row.popup);
    open_menu(
        world,
        menu.host,
        menu.source,
        path,
        MenuAnchor::Node(row_entity, PopupPlacement::Right),
        Some(row.popup),
        Some(row_entity),
    )
    .map(|h| h.root)
}

/// A row was activated (click, Enter, A): open its submenu or activate the item and close the
/// menu chain.
fn activate_row(world: &mut World, row_entity: Entity) {
    let Some(row) = world.get::<MenuRow>(row_entity).copied() else {
        return;
    };
    let Some(menu) = world.get::<MenuPopup>(row.popup).cloned() else {
        return;
    };
    let mut path = menu.path.clone();
    path.push(row.index);
    match activate_item(world, menu.host, menu.source, &path) {
        Ok(_) => {
            let root = chain_root(world, row.popup);
            close_popup(world, root);
        }
        Err(MenuError::HasSubmenu) => {
            open_submenu(world, row_entity);
        }
        Err(MenuError::Disabled | MenuError::NotFound) => {}
    }
}

/// The title entity of menu `index` of a menu bar.
fn bar_title(world: &World, bar: Entity, index: usize) -> Option<Entity> {
    world
        .get::<Children>(bar)?
        .iter()
        .find(|c| world.get::<MenuBarTitle>(*c).is_some_and(|t| t.0 == index))
}

/// Opens menu `index` of a menu bar below its title, closing the bar's other open menu.
pub fn open_bar_menu(world: &mut World, bar: Entity, index: usize) -> Option<PopupHandle> {
    for open in open_menus_of_host(world, bar) {
        close_popup(world, open);
    }
    let title = bar_title(world, bar, index)?;
    open_menu(
        world,
        bar,
        MenuSource::Bar(index),
        Vec::new(),
        MenuAnchor::Node(title, PopupPlacement::Below),
        None,
        Some(title),
    )
}

/// Ensures each [`MenuBar`] has one focusable title child per menu, labelled.
fn sync_menu_bars(world: &mut World) {
    let bars: Vec<(Entity, Vec<String>)> = world
        .query::<(Entity, &MenuBar)>()
        .iter(world)
        .map(|(e, b)| (e, b.menus.iter().map(|m| m.label.clone()).collect()))
        .collect();
    for (bar, labels) in bars {
        if world.get::<Classes>(bar).is_none() {
            world
                .entity_mut(bar)
                .insert((Classes(vec!["MenuBar".to_owned()]), Themed::default()));
        }
        let titles: Vec<Entity> = world
            .get::<Children>(bar)
            .map(|c| {
                c.iter()
                    .filter(|t| world.get::<MenuBarTitle>(*t).is_some())
                    .collect()
            })
            .unwrap_or_default();
        if titles.len() == labels.len() {
            for (title, label) in titles.into_iter().zip(labels) {
                let text_child = world
                    .get::<Children>(title)
                    .and_then(|c| c.iter().find(|e| world.get::<Text>(*e).is_some()));
                if let Some(text_child) = text_child
                    && let Some(mut text) = world.get_mut::<Text>(text_child)
                {
                    let current = text.spans.first().and_then(|s| match &s.content {
                        TextContent::Lit(t) => Some(t.as_str()),
                        _ => None,
                    });
                    if current != Some(label.as_str()) {
                        text.spans = vec![TextSpan::lit(label)];
                    }
                }
            }
            continue;
        }
        for title in titles {
            world.entity_mut(title).despawn();
        }
        for (i, label) in labels.iter().enumerate() {
            let style = row_style(world);
            let title = world
                .spawn((
                    UiNode,
                    NodeStyle::new(style),
                    Interactable::default(),
                    Focusable::default(),
                    WidgetSize::Sm,
                    MenuBarTitle(i),
                    Classes(vec!["MenuItem".to_owned()]),
                    Themed::default(),
                    ChildOf(bar),
                ))
                .id();
            label_node(world, title, label, false);
        }
        world.resource_mut::<crate::layout::LayoutDirty>().mark();
    }
}

/// Brings the rows of every open menu in step with the host lists (check marks, enabled state).
pub fn refresh_open_rows(world: &mut World) {
    let rows: Vec<(Entity, MenuRow)> = world
        .query::<(Entity, &MenuRow)>()
        .iter(world)
        .map(|(e, r)| (e, *r))
        .collect();
    for (entity, row) in rows {
        let Some(menu) = world.get::<MenuPopup>(row.popup).cloned() else {
            continue;
        };
        let mut path = menu.path.clone();
        path.push(row.index);
        let Some(item) = item_at(world, menu.host, menu.source, &path).cloned() else {
            continue;
        };
        let classes = row_classes(&item);
        if world.get::<Classes>(entity).is_some_and(|c| c.0 != classes) {
            world.entity_mut(entity).insert(Classes(classes));
        }
        let disabled = world.get::<Disabled>(entity).is_some();
        if disabled == item.enabled {
            if item.enabled {
                world.entity_mut(entity).remove::<Disabled>();
            } else {
                world.entity_mut(entity).insert(Disabled);
            }
        }
        let glyph = if item.checkable && item.checked {
            CHECK_GLYPH
        } else {
            ""
        };
        let children: Vec<Entity> = world
            .get::<Children>(entity)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        for child in children {
            if world.get::<MenuCheckGlyph>(child).is_some()
                && let Some(mut text) = world.get_mut::<Text>(child)
            {
                let current = text.spans.first().and_then(|s| match &s.content {
                    TextContent::Lit(t) => Some(t.as_str()),
                    _ => None,
                });
                if current != Some(glyph) {
                    text.spans = vec![TextSpan::lit(glyph)];
                }
            }
        }
    }
}

/// The nearest node at or above `entity` that hosts a [`ContextMenu`].
fn context_host(world: &World, entity: Entity) -> Option<Entity> {
    let mut current = entity;
    for _ in 0..4096 {
        if world.get::<ContextMenu>(current).is_some() {
            return Some(current);
        }
        current = world.get::<ChildOf>(current)?.0;
    }
    None
}

/// Opens `host`'s context menu at `anchor`, replacing an open one.
pub fn open_context_menu(world: &mut World, host: Entity, anchor: MenuAnchor) -> Option<PopupHandle> {
    for open in open_menus_of_host(world, host) {
        if world
            .get::<MenuPopup>(open)
            .is_some_and(|m| m.source == MenuSource::Context)
        {
            close_popup(world, open);
        }
    }
    let opener = match anchor {
        MenuAnchor::Node(node, _) => Some(node),
        MenuAnchor::Point(_) => world
            .get_resource::<FocusState>()
            .and_then(|f| f.focused)
            .or(Some(host)),
    };
    open_menu(world, host, MenuSource::Context, Vec::new(), anchor, None, opener)
}

/// Opens or closes `host`'s [`PopupMenu`].
fn toggle_popup_menu(world: &mut World, host: Entity) {
    if let Some(open) = open_menu_of(world, host, MenuSource::Popup, &[]) {
        close_popup(world, open);
        return;
    }
    let placement = world
        .get::<PopupMenu>(host)
        .map_or(PopupPlacement::Below, |m| m.placement);
    open_menu(
        world,
        host,
        MenuSource::Popup,
        Vec::new(),
        MenuAnchor::Node(host, placement),
        None,
        Some(host),
    );
}

fn rect_contains(rect: [f32; 4], p: [f32; 2]) -> bool {
    p[0] >= rect[0] && p[1] >= rect[1] && p[0] <= rect[0] + rect[2] && p[1] <= rect[1] + rect[3]
}

/// `ComputedNode.rect` under the window pointer: offset for a window-camera root, `None` for
/// a node under a camera texture or world panel (not reachable by the window pointer) or with
/// no area. `menu_hover` compares these against the window-space `UiPointer`, so they must go
/// through the same [`crate::interaction::window_rects`] rule as hit testing.
fn visible_rect(world: &World, entity: Entity) -> Option<[f32; 4]> {
    if crate::node::is_disabled(world, entity) {
        return None;
    }
    let hidden = world
        .get::<NodeStyle>(entity)
        .is_some_and(|s| s.0.display == Display::None);
    let computed = world
        .get::<ComputedNode>(entity)
        .filter(|c| !hidden && c.rect[2] > 0.0 && c.rect[3] > 0.0)?;
    let tag = world.get::<crate::world_target::UiTargetNode>(entity);
    crate::interaction::window_rects(computed.rect, computed.clip_rect, tag).map(|(rect, _)| rect)
}

/// Pointer moves over menus below the top one (where hit testing does not reach): over another
/// item of a parent menu, close the submenu (and open that item's); over another title of a menu
/// bar whose menu is open, switch to that menu.
fn menu_hover(world: &mut World) {
    let Some(p) = world.get_resource::<UiPointer>().and_then(|p| p.cursor) else {
        return;
    };
    let moved = {
        let mut state = world.resource_mut::<MenuInputState>();
        let moved = state.last_cursor != Some(p);
        state.last_cursor = Some(p);
        moved
    };
    if !moved {
        return;
    }
    let stack: Vec<Entity> = world
        .get_resource::<PopupStack>()
        .map(|s| s.popups().to_vec())
        .unwrap_or_default();
    let menus: Vec<Entity> = stack
        .iter()
        .copied()
        .filter(|e| world.get::<MenuPopup>(*e).is_some())
        .collect();
    let Some(&top) = menus.last() else {
        return;
    };
    // Menu bar: hovering another title switches menus.
    if let Some(root) = menus.first().copied()
        && let Some(MenuPopup {
            host,
            source: MenuSource::Bar(open_index),
            ..
        }) = world.get::<MenuPopup>(root).cloned()
    {
        let titles: Vec<(Entity, usize)> = world
            .get::<Children>(host)
            .map(|c| {
                c.iter()
                    .filter_map(|t| world.get::<MenuBarTitle>(t).map(|i| (t, i.0)))
                    .collect()
            })
            .unwrap_or_default();
        for (title, index) in titles {
            if index != open_index && visible_rect(world, title).is_some_and(|r| rect_contains(r, p)) {
                open_bar_menu(world, host, index);
                return;
            }
        }
    }
    // A parent menu under the pointer: the innermost one that is not the top.
    for (k, root) in menus.iter().enumerate().rev() {
        let Some(panel) = world.get::<Popup>(*root).map(|popup| popup.panel) else {
            continue;
        };
        if !visible_rect(world, panel).is_some_and(|r| rect_contains(r, p)) {
            continue;
        }
        if *root == top {
            return;
        }
        let rows: Vec<Entity> = world
            .query::<(Entity, &MenuRow)>()
            .iter(world)
            .filter(|(_, r)| r.popup == *root)
            .map(|(e, _)| e)
            .collect();
        let Some(row) = rows.into_iter().find(|r| {
            world.get::<Interactable>(*r).is_some()
                && visible_rect(world, *r).is_some_and(|rr| rect_contains(rr, p))
        }) else {
            return;
        };
        let child = menus.get(k + 1).copied();
        if child.and_then(|c| world.get::<Popup>(c)).and_then(|c| c.opener) == Some(row) {
            return;
        }
        close_children(world, *root);
        open_submenu(world, row);
        break;
    }
}

/// Filters each open menu by its search field's text.
fn menu_search(world: &mut World) {
    let fields: Vec<(Entity, Entity, String, String)> = world
        .query::<(Entity, &MenuSearchField, &TextInput)>()
        .iter(world)
        .map(|(e, f, t)| (e, f.popup, f.last.clone(), t.text.clone()))
        .collect();
    for (field, popup, last, text) in fields {
        if last == text {
            continue;
        }
        if let Some(mut f) = world.get_mut::<MenuSearchField>(field) {
            f.last = text.clone();
        }
        let query = text.to_lowercase();
        let Some(menu) = world.get::<MenuPopup>(popup).cloned() else {
            continue;
        };
        let rows: Vec<(Entity, usize)> = world
            .query::<(Entity, &MenuRow)>()
            .iter(world)
            .filter(|(_, r)| r.popup == popup)
            .map(|(e, r)| (e, r.index))
            .collect();
        for (row, index) in rows {
            let mut path = menu.path.clone();
            path.push(index);
            let show = match item_at(world, menu.host, menu.source, &path) {
                Some(item) => query.is_empty() || item.label.to_lowercase().contains(&query),
                None => query.is_empty(),
            };
            let display = if show { Display::Flex } else { Display::None };
            if let Some(mut style) = world.get_mut::<NodeStyle>(row)
                && style.0.display != display
            {
                style.0.display = display;
            }
        }
        world.resource_mut::<crate::layout::LayoutDirty>().mark();
    }
}

/// The first shown, enabled item row of `popup`, in entry order.
fn first_visible_row(world: &mut World, popup: Entity) -> Option<Entity> {
    let mut rows: Vec<(usize, Entity)> = world
        .query_filtered::<(Entity, &MenuRow), (With<Interactable>, Without<Disabled>)>()
        .iter(world)
        .filter(|(e, r)| r.popup == popup && !crate::node::is_disabled(world, *e))
        .map(|(e, r)| (r.index, e))
        .collect();
    rows.sort_by_key(|(i, _)| *i);
    rows.into_iter().map(|(_, e)| e).find(|e| {
        world
            .get::<NodeStyle>(*e)
            .is_none_or(|s| s.0.display != Display::None)
    })
}

/// `Update` (inside `ui_widgets`): menu bars, menu hosts, row activation, keyboard and gamepad
/// traversal, hover and search.
pub(crate) fn ui_menus(world: &mut World, input: &FrameInput) {
    let _span = tracing::trace_span!("ui_menus").entered();
    world.init_resource::<MenuActivations>();
    world.init_resource::<MenuInputState>();
    {
        // Drop what was already here at the previous run's start; what is here now (recorded
        // during the previous run or by an agent since) stays for this frame.
        let mut activations = world.resource_mut::<MenuActivations>();
        let stale = activations.stale.min(activations.activations.len());
        activations.activations.drain(..stale);
        activations.stale = activations.activations.len();
    }
    sync_menu_bars(world);
    // Filter before handling `Submitted`, so Enter in a search field picks from this frame's text.
    menu_search(world);
    let events: Vec<UiEvent> = world
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .cloned()
        .collect();
    let pointer_at = world.get_resource::<UiPointer>().and_then(UiPointer::position);
    for event in &events {
        let target = event.target;
        if world.get_entity(target).is_err() || crate::node::is_disabled(world, target) {
            continue;
        }
        match &event.kind {
            UiEventKind::Click => {
                if world.get::<MenuRow>(target).is_some() {
                    activate_row(world, target);
                } else if let Some(title) = world.get::<MenuBarTitle>(target).copied()
                    && let Some(bar) = world.get::<ChildOf>(target).map(|c| c.0)
                    && world.get::<MenuBar>(bar).is_some()
                {
                    if open_menu_of(world, bar, MenuSource::Bar(title.0), &[]).is_some() {
                        for open in open_menus_of_host(world, bar) {
                            close_popup(world, open);
                        }
                    } else {
                        open_bar_menu(world, bar, title.0);
                    }
                } else if world.get::<PopupMenu>(target).is_some() {
                    toggle_popup_menu(world, target);
                }
            }
            UiEventKind::ContextClick | UiEventKind::LongPress => {
                if let Some(host) = context_host(world, target) {
                    let anchor = match pointer_at {
                        Some(p) => MenuAnchor::Point(p),
                        None => MenuAnchor::Node(target, PopupPlacement::Below),
                    };
                    open_context_menu(world, host, anchor);
                }
            }
            UiEventKind::PointerEnter => {
                if let Some(row) = world.get::<MenuRow>(target).copied()
                    && crate::popup::top_interactive(world) == Some(row.popup)
                {
                    open_submenu(world, target);
                }
            }
            UiEventKind::Submitted(_) => {
                if let Some(field) = world.get::<MenuSearchField>(target).cloned()
                    && let Some(row) = first_visible_row(world, field.popup)
                {
                    activate_row(world, row);
                }
            }
            _ => {}
        }
    }
    menu_keys(world, input);
    menu_hover(world);
}

/// Left/Right in menus and the context-menu key.
fn menu_keys(world: &mut World, input: &FrameInput) {
    let focused = world
        .get_resource::<FocusState>()
        .and_then(|f| f.focused)
        .filter(|e| !crate::node::is_disabled(world, *e));
    let top = crate::popup::top_interactive(world);
    if (input.left || input.right)
        && let Some(focused) = focused
        && let Some(row) = world.get::<MenuRow>(focused).copied()
        && top == Some(row.popup)
        && let Some(menu) = world.get::<MenuPopup>(row.popup).cloned()
    {
        let mut path = menu.path.clone();
        path.push(row.index);
        let has_submenu = item_at(world, menu.host, menu.source, &path)
            .is_some_and(|item| item.enabled && !item.submenu.is_empty());
        let bar_step = |world: &World, forward: bool| -> Option<usize> {
            let MenuSource::Bar(i) = menu.source else {
                return None;
            };
            let n = world.get::<MenuBar>(menu.host)?.menus.len();
            (n > 1).then(|| if forward { (i + 1) % n } else { (i + n - 1) % n })
        };
        if input.right {
            if has_submenu {
                open_submenu(world, focused);
            } else if let Some(next) = bar_step(world, true) {
                open_bar_menu(world, menu.host, next);
            }
        } else if world.get::<Popup>(row.popup).and_then(|p| p.parent).is_some() {
            close_popup(world, row.popup);
        } else if let Some(prev) = bar_step(world, false) {
            open_bar_menu(world, menu.host, prev);
        }
        return;
    }
    if input.context
        && let Some(focused) = focused
        && crate::popup::popup_of(world, focused).is_none()
        && let Some(host) = context_host(world, focused)
    {
        open_context_menu(world, host, MenuAnchor::Node(focused, PopupPlacement::Below));
    }
}

// -------------------------------------------------------------------------------------------------
// Agent surface (`ui.open_popup`, `ui.close_popup`, `ui.menu`, `ui.menu_activate`), registered in
// `agent_bridge::call`. Book page `book/agents/ui-menus.md`.
// -------------------------------------------------------------------------------------------------

/// Finds the one node a call names (the bridge's own resolver).
pub type NodeResolver = fn(&mut World, &Value) -> Result<Entity, UiBridgeError>;

fn bridge_error(
    class: &'static str,
    reason: &'static str,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> UiBridgeError {
    UiBridgeError {
        class,
        reason,
        message: message.into(),
        hint: hint.into(),
    }
}

fn entry_json(
    world: &World,
    host: Entity,
    source: MenuSource,
    path: &[usize],
    index: usize,
    entry: &MenuEntry,
) -> Value {
    match entry {
        MenuEntry::Separator => json!({ "index": index, "separator": true }),
        MenuEntry::Item(item) => {
            let mut full = path.to_vec();
            full.push(index);
            let submenu: Vec<Value> = item
                .submenu
                .iter()
                .enumerate()
                .map(|(i, e)| entry_json(world, host, source, &full, i, e))
                .collect();
            json!({
                "index": index,
                "separator": false,
                "id": item.key(),
                "label": item.label,
                "path": path_keys(world, host, source, &full),
                "enabled": item.enabled,
                "checkable": item.checkable,
                "checked": item.checked,
                "shortcut": shortcut_text(world, item),
                "submenu": submenu,
            })
        }
    }
}

fn popup_json(world: &World, root: Entity) -> Value {
    let Some(popup) = world.get::<Popup>(root) else {
        return Value::Null;
    };
    let mut out = Map::new();
    out.insert("popup".into(), json!(root.to_bits()));
    out.insert("panel".into(), json!(popup.panel.to_bits()));
    out.insert("kind".into(), json!(popup.kind.name()));
    out.insert("anchor".into(), json!(popup.anchor.map(Entity::to_bits)));
    out.insert("opener".into(), json!(popup.opener.map(Entity::to_bits)));
    out.insert("parent".into(), json!(popup.parent.map(Entity::to_bits)));
    out.insert("interactive".into(), json!(popup.interactive));
    out.insert("placement".into(), json!(popup.placement.name()));
    match popup.placed {
        Some(placed) => {
            out.insert("placed".into(), json!(placed.placement.name()));
            out.insert("rect".into(), json!(placed.rect));
            out.insert("flipped".into(), json!(placed.flipped));
            out.insert("shifted".into(), json!(placed.shifted));
        }
        None => {
            out.insert("placed".into(), Value::Null);
        }
    }
    if let Some(menu) = world.get::<MenuPopup>(root) {
        out.insert("host".into(), json!(menu.host.to_bits()));
        out.insert(
            "menu_path".into(),
            json!(path_keys(world, menu.host, menu.source, &menu.path)),
        );
    }
    Value::Object(out)
}

fn stack_json(world: &World) -> Value {
    let stack: Vec<Value> = world
        .get_resource::<PopupStack>()
        .map(|s| s.popups().iter().map(|p| popup_json(world, *p)).collect())
        .unwrap_or_default();
    Value::Array(stack)
}

/// The menu list a node hosts, in precedence order (menu bar, popup menu, context menu).
fn host_source(world: &World, node: Entity, which: Option<&str>) -> Option<MenuSource> {
    match which {
        Some("popup" | "popup_menu") => world.get::<PopupMenu>(node).map(|_| MenuSource::Popup),
        Some("context" | "context_menu") => world.get::<ContextMenu>(node).map(|_| MenuSource::Context),
        _ => {
            if world.get::<MenuBar>(node).is_some() {
                Some(MenuSource::Bar(0))
            } else if world.get::<PopupMenu>(node).is_some() {
                Some(MenuSource::Popup)
            } else if world.get::<ContextMenu>(node).is_some() {
                Some(MenuSource::Context)
            } else {
                None
            }
        }
    }
}

/// `ui.open_popup {anchor, popup?, placement?}`: opens what the anchor node hosts.
pub fn agent_open_popup(
    world: &mut World,
    params: &Value,
    resolve: NodeResolver,
) -> Result<Value, UiBridgeError> {
    let anchor = match params.get("anchor").filter(|v| !v.is_null()) {
        Some(Value::Object(filter)) => resolve(world, &Value::Object(filter.clone()))?,
        Some(id) => resolve(world, &json!({ "node": id }))?,
        None => resolve(world, params)?,
    };
    let popup = match params.get("popup").filter(|v| !v.is_null()) {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(_) => {
            return Err(bridge_error(
                "invalid_params",
                "ui_bad_param",
                "`popup` must be a string",
                "dropdown, menu, context, tooltip, or a menu bar title",
            ));
        }
    };
    let placement = match params.get("placement").and_then(Value::as_str) {
        None => None,
        Some(name) => Some(PopupPlacement::parse(name).ok_or_else(|| {
            bridge_error(
                "invalid_params",
                "ui_bad_param",
                format!("`placement` must be below, above, left or right, not `{name}`"),
                "omit it for the widget's own side",
            )
        })?),
    };
    let want = popup.as_deref();
    let unavailable = || {
        bridge_error(
            "rejected",
            "ui_popup_unavailable",
            "the anchor hosts no popup of that kind",
            "anchor a Dropdown, a Tooltip host, a PopupMenu, ContextMenu or MenuBar node (see ui.menu)",
        )
    };
    let existing_before: Vec<Entity> = world
        .get_resource::<PopupStack>()
        .map(|s| s.popups().to_vec())
        .unwrap_or_default();
    let root = if world.get::<Dropdown>(anchor).is_some() && matches!(want, None | Some("dropdown")) {
        crate::widgets::open_dropdown_list(world, anchor)
    } else if world.get::<crate::widgets::Tooltip>(anchor).is_some() && matches!(want, None | Some("tooltip"))
    {
        crate::widgets::pin_tooltip(world, anchor)
    } else if let Some(bar) = world.get::<MenuBar>(anchor).cloned()
        && !matches!(want, Some("menu" | "popup" | "context" | "dropdown" | "tooltip"))
    {
        let index = match want {
            None => Some(0),
            Some(name) => bar
                .menus
                .iter()
                .position(|m| m.key() == name || m.label.eq_ignore_ascii_case(name))
                .or_else(|| name.parse::<usize>().ok().filter(|i| *i < bar.menus.len())),
        }
        .ok_or_else(|| {
            bridge_error(
                "not_found",
                "ui_menu_not_found",
                format!("the menu bar has no menu `{}`", want.unwrap_or_default()),
                "name a title, an id or an index from ui.menu",
            )
        })?;
        match open_menu_of(world, anchor, MenuSource::Bar(index), &[]) {
            Some(open) => Some(open),
            None => open_bar_menu(world, anchor, index).map(|h| h.root),
        }
    } else if world.get::<PopupMenu>(anchor).is_some() && matches!(want, None | Some("menu" | "popup")) {
        match open_menu_of(world, anchor, MenuSource::Popup, &[]) {
            Some(open) => Some(open),
            None => {
                let side = placement.unwrap_or_else(|| {
                    world
                        .get::<PopupMenu>(anchor)
                        .map_or(PopupPlacement::Below, |m| m.placement)
                });
                open_menu(
                    world,
                    anchor,
                    MenuSource::Popup,
                    Vec::new(),
                    MenuAnchor::Node(anchor, side),
                    None,
                    Some(anchor),
                )
                .map(|h| h.root)
            }
        }
    } else if world.get::<ContextMenu>(anchor).is_some() && matches!(want, None | Some("context")) {
        open_context_menu(
            world,
            anchor,
            MenuAnchor::Node(anchor, placement.unwrap_or(PopupPlacement::Below)),
        )
        .map(|h| h.root)
    } else {
        return Err(unavailable());
    };
    let root = root.ok_or_else(unavailable)?;
    if let Some(side) = placement
        && let Some(mut popup) = world.get_mut::<Popup>(root)
    {
        popup.placement = side;
    }
    let mut out = popup_json(world, root);
    if let Some(map) = out.as_object_mut() {
        map.insert("already_open".into(), json!(existing_before.contains(&root)));
        map.insert("stack".into(), stack_json(world));
    }
    Ok(out)
}

/// `ui.close_popup {popup?}`: closes one popup (the top one when omitted) and its submenus.
pub fn agent_close_popup(world: &mut World, params: &Value) -> Result<Value, UiBridgeError> {
    let stack: Vec<Entity> = world
        .get_resource::<PopupStack>()
        .map(|s| s.popups().to_vec())
        .unwrap_or_default();
    let target = match params.get("popup").filter(|v| !v.is_null()) {
        None => *stack.last().ok_or_else(|| {
            bridge_error(
                "rejected",
                "ui_no_popup_open",
                "no popup is open",
                "open one with ui.open_popup",
            )
        })?,
        Some(v) => {
            let bits = v.as_u64().ok_or_else(|| {
                bridge_error(
                    "invalid_params",
                    "ui_bad_param",
                    "`popup` must be a popup id",
                    "take `popup` from ui.open_popup or the `stack` list",
                )
            })?;
            let root = stack.iter().copied().find(|p| {
                p.to_bits() == bits
                    || world
                        .get::<Popup>(*p)
                        .is_some_and(|popup| popup.panel.to_bits() == bits)
            });
            root.ok_or_else(|| {
                bridge_error(
                    "not_found",
                    "ui_popup_not_found",
                    format!("{bits} is not an open popup"),
                    "list open popups with the `stack` of ui.open_popup or ui.close_popup",
                )
            })?
        }
    };
    if world
        .get::<Popup>(target)
        .is_some_and(|p| p.kind == PopupKind::Tooltip)
        && let Some(anchor) = world.get::<Popup>(target).and_then(|p| p.anchor)
    {
        crate::widgets::unpin_tooltip(world, anchor);
    }
    let closed = close_popup(world, target);
    let focus = world.get_resource::<FocusState>().and_then(|f| f.focused);
    Ok(json!({
        "closed": closed.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
        "focus": focus.map(Entity::to_bits),
        "stack": stack_json(world),
    }))
}

/// The host and list a `ui.menu` / `ui.menu_activate` node names: a host, or an open menu's
/// popup root, panel or row.
fn agent_menu_target(
    world: &World,
    node: Entity,
    which: Option<&str>,
) -> Option<(Entity, Option<MenuSource>)> {
    if let Some(source) = host_source(world, node, which) {
        return Some((node, Some(source)));
    }
    if world.get::<Dropdown>(node).is_some() {
        return Some((node, None));
    }
    let popup = world
        .get::<MenuRow>(node)
        .map(|r| r.popup)
        .or_else(|| crate::popup::popup_of(world, node))?;
    let menu = world.get::<MenuPopup>(popup)?;
    Some((menu.host, Some(menu.source)))
}

fn menu_not_found() -> UiBridgeError {
    bridge_error(
        "not_found",
        "ui_menu_not_found",
        "the node hosts no menu",
        "pass a MenuBar, PopupMenu, ContextMenu or Dropdown node (or an open menu's popup)",
    )
}

/// `ui.menu {node, menu?}`: the node's menu entries with their state.
pub fn agent_menu(world: &mut World, params: &Value, resolve: NodeResolver) -> Result<Value, UiBridgeError> {
    let node = resolve(world, params)?;
    let which = params.get("menu").and_then(Value::as_str);
    let (host, source) = agent_menu_target(world, node, which).ok_or_else(menu_not_found)?;
    let world: &World = world;
    let Some(source) = source else {
        // A dropdown: its options as checkable items, the selected one checked.
        let dd = world.get::<Dropdown>(host).ok_or_else(menu_not_found)?;
        let items: Vec<Value> = dd
            .options
            .iter()
            .enumerate()
            .map(|(i, label)| {
                json!({
                    "index": i, "separator": false, "id": label, "label": label, "path": [label],
                    "enabled": true, "checkable": true, "checked": dd.selected == Some(i),
                    "shortcut": "", "submenu": [],
                })
            })
            .collect();
        let open = world.get_resource::<PopupStack>().and_then(|s| {
            s.popups().iter().copied().find(|p| {
                world
                    .get::<Popup>(*p)
                    .is_some_and(|popup| popup.kind == PopupKind::Dropdown && popup.anchor == Some(host))
            })
        });
        return Ok(json!({
            "node": host.to_bits(), "kind": "dropdown", "open": open.is_some(),
            "popup": open.map(Entity::to_bits), "items": items, "stack": stack_json(world),
        }));
    };
    let (items, open) = if let MenuSource::Bar(_) = source {
        let bar = world.get::<MenuBar>(host).ok_or_else(menu_not_found)?;
        let items: Vec<Value> = bar
            .menus
            .iter()
            .enumerate()
            .map(|(i, menu)| {
                let submenu: Vec<Value> = menu
                    .submenu
                    .iter()
                    .enumerate()
                    .map(|(k, e)| entry_json(world, host, MenuSource::Bar(i), &[], k, e))
                    .collect();
                json!({
                    "index": i, "separator": false, "id": menu.key(), "label": menu.label,
                    "path": [menu.key()], "enabled": menu.enabled, "checkable": false, "checked": false,
                    "shortcut": menu.shortcut, "submenu": submenu,
                })
            })
            .collect();
        let open = open_menus_of_host(world, host).into_iter().next();
        (items, open)
    } else {
        let entries = entries_at(world, host, source, &[]).unwrap_or_default();
        let items: Vec<Value> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| entry_json(world, host, source, &[], i, e))
            .collect();
        (items, open_menu_of(world, host, source, &[]))
    };
    Ok(json!({
        "node": host.to_bits(),
        "kind": source.kind_name(),
        "open": open.is_some(),
        "popup": open.map(Entity::to_bits),
        "items": items,
        "stack": stack_json(world),
    }))
}

/// Parses a `path`: a list of keys, labels or indices, or one `/`-joined string.
fn parse_path(params: &Value) -> Result<Vec<String>, UiBridgeError> {
    let bad = || {
        bridge_error(
            "invalid_params",
            "ui_bad_param",
            "`path` must be a list of item ids, labels or indices, or one string joined with `/`",
            "e.g. [\"view\", \"grid\"] or \"view/grid\"; see ui.menu for the paths",
        )
    };
    let segments: Vec<String> = match params.get("path").filter(|v| !v.is_null()) {
        None => return Err(bad()),
        Some(Value::String(s)) => s.split('/').map(str::to_owned).collect(),
        Some(Value::Array(list)) => list
            .iter()
            .map(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(bad)?,
        Some(_) => return Err(bad()),
    };
    if segments.is_empty() || segments.iter().any(String::is_empty) {
        return Err(bad());
    }
    Ok(segments)
}

/// The index of the entry `segment` names: an exact key, then a case-insensitive label, then an
/// index.
fn find_segment<'a>(
    names: impl Iterator<Item = (usize, &'a str, &'a str)> + Clone,
    segment: &str,
) -> Option<usize> {
    names
        .clone()
        .find(|(_, key, _)| *key == segment)
        .or_else(|| {
            names
                .clone()
                .find(|(_, _, label)| label.eq_ignore_ascii_case(segment))
        })
        .map(|(i, _, _)| i)
        .or_else(|| {
            segment
                .parse::<usize>()
                .ok()
                .filter(|i| names.clone().any(|(k, _, _)| k == *i))
        })
}

fn entry_names(entries: &[MenuEntry]) -> impl Iterator<Item = (usize, &str, &str)> + Clone {
    entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.item().map(|item| (i, item.key(), item.label.as_str())))
}

/// `ui.menu_activate {node, path, menu?}`: activates a menu item as a click would.
pub fn agent_menu_activate(
    world: &mut World,
    params: &Value,
    resolve: NodeResolver,
) -> Result<Value, UiBridgeError> {
    let node = resolve(world, params)?;
    let which = params.get("menu").and_then(Value::as_str);
    let segments = parse_path(params)?;
    let (host, source) = agent_menu_target(world, node, which).ok_or_else(menu_not_found)?;
    let path_missing = |segment: &str| {
        bridge_error(
            "not_found",
            "ui_menu_path_not_found",
            format!("no menu item `{segment}` at that point of the path"),
            "list the items and their paths with ui.menu",
        )
    };
    let Some(source) = source else {
        let options: Vec<String> = world
            .get::<Dropdown>(host)
            .map(|d| d.options.clone())
            .unwrap_or_default();
        let [segment] = segments.as_slice() else {
            return Err(path_missing(segments.last().map_or("", String::as_str)));
        };
        let index = find_segment(
            options
                .iter()
                .enumerate()
                .map(|(i, o)| (i, o.as_str(), o.as_str())),
            segment,
        )
        .ok_or_else(|| path_missing(segment))?;
        let closed = crate::widgets::select_dropdown(world, host, index);
        return Ok(json!({
            "node": host.to_bits(), "path": [options[index].clone()], "id": options[index].clone(),
            "checked": true, "closed": closed.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
        }));
    };
    let mut rest: &[String] = &segments;
    let source = match source {
        MenuSource::Bar(_) => {
            let bar = world.get::<MenuBar>(host).ok_or_else(menu_not_found)?;
            let Some((first, tail)) = rest.split_first() else {
                return Err(path_missing(""));
            };
            let index = find_segment(
                bar.menus
                    .iter()
                    .enumerate()
                    .map(|(i, m)| (i, m.key(), m.label.as_str())),
                first,
            )
            .ok_or_else(|| path_missing(first))?;
            rest = tail;
            MenuSource::Bar(index)
        }
        other => other,
    };
    let mut path: Vec<usize> = Vec::with_capacity(rest.len());
    for segment in rest {
        let entries = entries_at(world, host, source, &path).ok_or_else(|| path_missing(segment))?;
        let index = find_segment(entry_names(entries), segment).ok_or_else(|| path_missing(segment))?;
        path.push(index);
    }
    let activation = activate_item(world, host, source, &path).map_err(|e| match e {
        MenuError::NotFound => path_missing(segments.last().map_or("", String::as_str)),
        MenuError::Disabled => bridge_error(
            "rejected",
            "ui_menu_item_disabled",
            "the menu item is disabled",
            "check `enabled` in ui.menu",
        ),
        MenuError::HasSubmenu => bridge_error(
            "rejected",
            "ui_menu_item_has_submenu",
            "the menu item opens a submenu; activate one of its items",
            "add the submenu item to `path` (see `submenu` in ui.menu)",
        ),
    })?;
    // As a click would: the host's open menus close.
    let mut closed = Vec::new();
    for open in open_menus_of_host(world, host) {
        closed.extend(close_popup(world, open));
    }
    Ok(json!({
        "node": host.to_bits(),
        "path": activation.path,
        "id": activation.path.last(),
        "checked": activation.checked,
        "closed": closed.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
    }))
}

/// Whether `entity` belongs to an open menu (a row, the search field, the panel).
pub fn in_open_menu(world: &World, entity: Entity) -> bool {
    crate::popup::popup_of(world, entity).is_some_and(|p| world.get::<MenuPopup>(p).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn type_ahead_cycles_one_letter() {
        let o = options(&["Apple", "Banana", "Avocado", "Cherry", "apricot"]);
        assert_eq!(type_ahead(&o, None, "a"), Some(0));
        assert_eq!(type_ahead(&o, Some(0), "a"), Some(2));
        assert_eq!(type_ahead(&o, Some(2), "a"), Some(4), "case-insensitive");
        assert_eq!(type_ahead(&o, Some(4), "a"), Some(0), "wraps");
        assert_eq!(
            type_ahead(&o, Some(0), "aa"),
            Some(2),
            "a repeated letter still cycles"
        );
        assert_eq!(type_ahead(&o, Some(1), "z"), None);
        assert_eq!(type_ahead(&o, Some(1), ""), None);
        assert_eq!(type_ahead(&[], None, "a"), None);
    }

    #[test]
    fn type_ahead_prefix_keeps_a_matching_current() {
        let o = options(&["Apple", "Banana", "Avocado", "Cherry", "apricot"]);
        assert_eq!(type_ahead(&o, Some(0), "av"), Some(2));
        assert_eq!(type_ahead(&o, Some(2), "avo"), Some(2), "current still matches");
        assert_eq!(
            type_ahead(&o, Some(3), "ap"),
            Some(4),
            "searches from the current one"
        );
        assert_eq!(type_ahead(&o, Some(1), "ch"), Some(3));
        assert_eq!(type_ahead(&o, Some(1), "cx"), None);
    }

    #[test]
    fn segments_match_key_then_label_then_index() {
        let entries = vec![
            MenuEntry::Item(MenuItem::new("open", "Open file")),
            MenuEntry::Separator,
            MenuEntry::Item(MenuItem::new("", "Quit")),
            MenuEntry::Item(MenuItem::new("2", "Two")),
        ];
        assert_eq!(find_segment(entry_names(&entries), "open"), Some(0));
        assert_eq!(find_segment(entry_names(&entries), "open FILE"), Some(0));
        assert_eq!(
            find_segment(entry_names(&entries), "Quit"),
            Some(2),
            "empty id uses the label"
        );
        assert_eq!(
            find_segment(entry_names(&entries), "2"),
            Some(3),
            "a key wins over an index"
        );
        assert_eq!(find_segment(entry_names(&entries), "0"), Some(0));
        assert_eq!(
            find_segment(entry_names(&entries), "1"),
            None,
            "a separator is not an item"
        );
        assert_eq!(find_segment(entry_names(&entries), "nope"), None);
    }

    #[test]
    fn paths_parse_from_lists_and_strings() {
        assert_eq!(
            parse_path(&json!({ "path": "view/grid" })).unwrap(),
            vec!["view".to_owned(), "grid".to_owned()]
        );
        assert_eq!(
            parse_path(&json!({ "path": ["view", 2] })).unwrap(),
            vec!["view".to_owned(), "2".to_owned()]
        );
        for bad in [
            json!({}),
            json!({ "path": "" }),
            json!({ "path": [true] }),
            json!({ "path": 3 }),
        ] {
            assert_eq!(parse_path(&bad).unwrap_err().reason, "ui_bad_param");
        }
    }

    #[test]
    fn item_paths_reach_into_submenus() {
        let mut world = World::new();
        let host = world
            .spawn(PopupMenu {
                entries: vec![
                    MenuItem::new("file", "File")
                        .with_submenu(vec![MenuItem::new("recent", "Recent").checkable(false).into()])
                        .into(),
                    MenuEntry::Separator,
                    MenuItem::new("quit", "Quit").disabled().into(),
                ],
                ..Default::default()
            })
            .id();
        assert_eq!(
            item_at(&world, host, MenuSource::Popup, &[0, 0]).map(|i| i.id.as_str()),
            Some("recent")
        );
        assert!(
            item_at(&world, host, MenuSource::Popup, &[1]).is_none(),
            "separator"
        );
        assert_eq!(
            path_keys(&world, host, MenuSource::Popup, &[0, 0]),
            vec!["file".to_owned(), "recent".to_owned()]
        );
        world.init_resource::<Messages<UiEvent>>();
        assert_eq!(
            activate_item(&mut world, host, MenuSource::Popup, &[0]),
            Err(MenuError::HasSubmenu)
        );
        assert_eq!(
            activate_item(&mut world, host, MenuSource::Popup, &[2]),
            Err(MenuError::Disabled)
        );
        let done = activate_item(&mut world, host, MenuSource::Popup, &[0, 0]).unwrap();
        assert_eq!(done.checked, Some(true));
        assert!(item_at(&world, host, MenuSource::Popup, &[0, 0]).is_some_and(|i| i.checked));
        assert_eq!(world.resource::<MenuActivations>().activations.len(), 1);
    }

    #[test]
    fn bar_hover_switches_menus_in_window_space() {
        // Audit ui1 correctness 5: menu_hover compares the window-space UiPointer to the raw
        // ComputedNode.rect; a bar in a camera viewport needs the window_origin offset, and a
        // local-space hit must not switch menus.
        let mut world = World::new();
        world.init_resource::<Messages<UiEvent>>();
        world.init_resource::<MenuInputState>();
        world.init_resource::<UiPointer>();
        world.init_resource::<crate::layout::LayoutDirty>();
        let bar = world
            .spawn((
                UiNode,
                MenuBar {
                    menus: vec![
                        MenuItem::new("file", "File")
                            .with_submenu(vec![MenuItem::new("open", "Open").into()]),
                        MenuItem::new("edit", "Edit")
                            .with_submenu(vec![MenuItem::new("copy", "Copy").into()]),
                    ],
                },
            ))
            .id();
        let spawn_title = |world: &mut World, index: usize, rect: [f32; 4]| {
            let title = world
                .spawn((
                    UiNode,
                    MenuBarTitle(index),
                    ComputedNode {
                        rect,
                        clip_rect: rect,
                        scale: 1.0,
                        ..Default::default()
                    },
                    crate::world_target::UiTargetNode {
                        root: bar,
                        window_origin: Some([960.0, 0.0]),
                    },
                ))
                .id();
            world.entity_mut(bar).add_child(title);
        };
        // Local rects under a root at window origin (960, 0).
        spawn_title(&mut world, 0, [40.0, 0.0, 80.0, 24.0]);
        spawn_title(&mut world, 1, [140.0, 0.0, 80.0, 24.0]);
        let source = |world: &World| {
            world
                .resource::<PopupStack>()
                .top()
                .and_then(|p| world.get::<MenuPopup>(p))
                .map(|m| m.source)
        };
        open_bar_menu(&mut world, bar, 0).expect("menu opens");
        assert_eq!(source(&world), Some(MenuSource::Bar(0)));
        // Window point over the second title (local [140..220] plus 960).
        world.resource_mut::<UiPointer>().cursor = Some([1140.0, 10.0]);
        menu_hover(&mut world);
        assert_eq!(
            source(&world),
            Some(MenuSource::Bar(1)),
            "hovering the drawn second title switches the menu"
        );
        // A point that matches only the un-offset local rect must not switch.
        open_bar_menu(&mut world, bar, 0).expect("menu reopens");
        world.resource_mut::<UiPointer>().cursor = Some([190.0, 10.0]);
        menu_hover(&mut world);
        assert_eq!(
            source(&world),
            Some(MenuSource::Bar(0)),
            "the pointer at (190, 10) is not over any drawn title"
        );
    }

    #[test]
    fn bar_paths_start_with_the_menu_key() {
        let mut world = World::new();
        let host = world
            .spawn(MenuBar {
                menus: vec![
                    MenuItem::new("view", "View").with_submenu(vec![MenuItem::new("grid", "Grid").into()]),
                ],
            })
            .id();
        assert_eq!(
            path_keys(&world, host, MenuSource::Bar(0), &[0]),
            vec!["view".to_owned(), "grid".to_owned()]
        );
    }
}
