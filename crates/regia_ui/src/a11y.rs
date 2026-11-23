// SPDX-License-Identifier: MPL-2.0
//! Accessibility tree export and screen-reader hooks (UI L14, ADR-865; roadmap L03).
//!
//! The runtime UI has no platform accessibility layer of its own, so this module builds a
//! platform-neutral semantic tree from the widget tree and hands changes to a
//! [`ScreenReaderBackend`]:
//!
//! - [`build_a11y_tree`] walks every [`UiRoot`] and infers a role, an accessible name, a value and
//!   states for each node from the widget components ([`Button`], [`Toggle`], [`Slider`], …). A
//!   childless [`Text`] that labels its parent is folded into the parent's name, and an unnamed
//!   plain container is flattened (its children move up), so the tree reads like the screen.
//!   [`Accessible`] overrides the inferred role, name and description, hides a node, or marks it a
//!   live region: this is the hook custom widgets use.
//! - [`A11yTree::diff`] turns two snapshots into [`A11yEvent`]s (added, removed, name, value, states,
//!   focus, announcements). A live region's name or value change becomes an announcement.
//! - [`ScreenReader`] (a resource) owns the backend and the last tree. [`sync_screen_reader`] rebuilds
//!   the tree when the reader becomes live and when the widgets, focus, input layer or locale change.
//!   A quiet frame keeps the stored tree. A game without a screen reader pays nothing.
//! - [`atspi::AtSpiStub`] maps the tree onto AT-SPI 2 roles and the D-Bus signals a Linux bridge
//!   would emit, and records them; it does not talk to D-Bus (no dependency for it yet). It is the
//!   seam a real bridge, and later UI Automation (Windows) and Android `AccessibilityNodeInfo`
//!   backends, plug into.
//!
//! - Requests flow the other way too (ADR-1621): a reader asks to press, raise, lower, set text,
//!   focus, close, scroll a node into view or page a scroll view ([`A11yRequest`]). A backend queues
//!   them ([`ScreenReaderBackend::drain_requests`]); [`route_a11y_requests`] checks each against the
//!   tree and sends it down the same path a key press or pointer click takes ([`apply_request`]),
//!   rejecting disabled, inert and unknown nodes with an [`A11yReject`] reason. [`RecordingReader`]
//!   is a headless backend for tests. The OS bridge (`accesskit` feature, ADR-1998) plugs in at
//!   [`ScreenReaderBackend`]: `crate::accesskit_bridge` turns the tree into AccessKit tree updates
//!   for the adapter `regia_platform` attaches to the window (AT-SPI, UI Automation,
//!   NSAccessibility) and turns the adapter's action requests into [`A11yRequest`]s.
//! - Override labels, descriptions and usage hints may be localization keys, re-resolved on every
//!   rebuild, so a locale change renames the node and the diff reports it.
//!
//! Agent methods: `ui.a11y_tree`, `ui.a11y_announce` and `ui.a11y_act` (book `agents/ui-a11y.md`).

#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

#[cfg(test)]
thread_local! {
    /// Times `build_a11y_tree` ran. A quiet live reader, and a request served from the stored
    /// tree, must not increase this. Per test thread, so a parallel test cannot bump another count.
    static A11Y_TREE_BUILDS: Cell<u64> = const { Cell::new(0) };
}

/// How many times this thread built an accessibility tree.
#[cfg(test)]
fn a11y_tree_builds() -> u64 {
    A11Y_TREE_BUILDS.with(Cell::get)
}

/// Why the last [`a11y_inputs_dirty`] call matched (empty when the frame was quiet).
#[cfg(test)]
static A11Y_DIRTY_REASON: Mutex<String> = Mutex::new(String::new());

use regia_app::prelude::*;
use regia_ecs::ecs::change_detection::Tick;
use regia_ecs::ecs::component::ComponentId;
use regia_ecs::ecs::lifecycle::RemovedComponentEntity;
use regia_ecs::ecs::message::MessageCursor;

use crate::default_widgets::Breadcrumbs;
use crate::focus::Focusable;
use crate::interaction::{UiEvent, UiEventKind};
use crate::localization::Localization;
use crate::node::{ComputedNode, Disabled, Focused, Invalid, NodeStyle, Selected, UiImage, UiRoot};
use crate::radial::RadialMenu;
use crate::scroll::{ListView, ScrollView};
use crate::style::Display;
use crate::text::{Text, TextContent};
use crate::widgets::{Button, Dropdown, Modal, ProgressBar, Slider, TabBar, TextInput, Toggle};

/// What a node is, as assistive technology sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum A11yRole {
    /// A [`UiRoot`].
    Window,
    /// A container with no other role.
    Group,
    /// [`Button`].
    Button,
    /// [`Toggle`].
    CheckBox,
    /// [`Slider`].
    Slider,
    /// [`ProgressBar`].
    ProgressBar,
    /// [`TextInput`].
    TextInput,
    /// [`Dropdown`].
    ComboBox,
    /// [`TabBar`].
    TabList,
    /// [`Modal`].
    Dialog,
    /// [`ListView`].
    List,
    /// [`ScrollView`].
    ScrollView,
    /// [`Text`] on its own.
    Label,
    /// [`UiImage`].
    Image,
    /// [`RadialMenu`].
    Menu,
    /// A heading (only through [`Accessible::role`]).
    Heading,
    /// A landmark region (only through [`Accessible::role`]).
    Region,
    /// A status message (only through [`Accessible::role`]); usually also a live region.
    Status,
    /// An urgent message (only through [`Accessible::role`]).
    Alert,
}

impl A11yRole {
    /// Every role, in declaration order.
    pub const ALL: [Self; 19] = [
        Self::Window,
        Self::Group,
        Self::Button,
        Self::CheckBox,
        Self::Slider,
        Self::ProgressBar,
        Self::TextInput,
        Self::ComboBox,
        Self::TabList,
        Self::Dialog,
        Self::List,
        Self::ScrollView,
        Self::Label,
        Self::Image,
        Self::Menu,
        Self::Heading,
        Self::Region,
        Self::Status,
        Self::Alert,
    ];

    /// The protocol name (`snake_case`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Group => "group",
            Self::Button => "button",
            Self::CheckBox => "check_box",
            Self::Slider => "slider",
            Self::ProgressBar => "progress_bar",
            Self::TextInput => "text_input",
            Self::ComboBox => "combo_box",
            Self::TabList => "tab_list",
            Self::Dialog => "dialog",
            Self::List => "list",
            Self::ScrollView => "scroll_view",
            Self::Label => "label",
            Self::Image => "image",
            Self::Menu => "menu",
            Self::Heading => "heading",
            Self::Region => "region",
            Self::Status => "status",
            Self::Alert => "alert",
        }
    }

    /// Parses [`Self::name`].
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.name() == name)
    }

    /// How the role is spoken in a summary ("check box").
    pub fn spoken(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Group => "group",
            Self::Button => "button",
            Self::CheckBox => "check box",
            Self::Slider => "slider",
            Self::ProgressBar => "progress bar",
            Self::TextInput => "text field",
            Self::ComboBox => "combo box",
            Self::TabList => "tab list",
            Self::Dialog => "dialog",
            Self::List => "list",
            Self::ScrollView => "scroll area",
            Self::Label => "text",
            Self::Image => "image",
            Self::Menu => "menu",
            Self::Heading => "heading",
            Self::Region => "region",
            Self::Status => "status",
            Self::Alert => "alert",
        }
    }
}

/// How urgently a live region or an announcement interrupts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Politeness {
    /// Not a live region: changes are not announced.
    #[default]
    Off,
    /// Announced when the reader is idle.
    Polite,
    /// Interrupts what is being read.
    Assertive,
}

impl Politeness {
    /// The protocol name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Polite => "polite",
            Self::Assertive => "assertive",
        }
    }

    /// Parses [`Self::name`].
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Off, Self::Polite, Self::Assertive]
            .into_iter()
            .find(|p| p.name() == name)
    }
}

/// Accessibility overrides for one node: the custom-widget hook. Every field is optional; what is
/// unset is inferred from the widget components.
///
/// ```
/// use regia_ui::a11y::{A11yRole, Accessible, Politeness};
///
/// let score = Accessible::default()
///     .with_role(A11yRole::Status)
///     .with_label("Score")
///     .with_live(Politeness::Polite);
/// assert_eq!(score.role, Some(A11yRole::Status));
/// ```
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct Accessible {
    /// The role, instead of the inferred one.
    pub role: Option<A11yRole>,
    /// The accessible name, instead of the node's text.
    pub label: Option<String>,
    /// A longer description (read after the name).
    pub description: Option<String>,
    /// A localization key for the name; wins over [`Self::label`] when the key is defined (and is
    /// the only source when `label` is unset).
    pub label_key: Option<String>,
    /// A localization key for the description; same rule as [`Self::label_key`].
    pub description_key: Option<String>,
    /// A short usage hint read after the description ("Double tap to open").
    pub hint: Option<String>,
    /// A localization key for the hint; same rule as [`Self::label_key`].
    pub hint_key: Option<String>,
    /// Leave the node and its subtree out of the tree (decoration).
    pub hidden: bool,
    /// Announce changes of the name or value.
    pub live: Politeness,
}

impl Accessible {
    /// Sets the role.
    #[must_use]
    pub fn with_role(mut self, role: A11yRole) -> Self {
        self.role = Some(role);
        self
    }

    /// Sets the accessible name.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Sets the description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Sets the localization key of the name.
    #[must_use]
    pub fn with_label_key(mut self, key: impl Into<String>) -> Self {
        self.label_key = Some(key.into());
        self
    }

    /// Sets the localization key of the description.
    #[must_use]
    pub fn with_description_key(mut self, key: impl Into<String>) -> Self {
        self.description_key = Some(key.into());
        self
    }

    /// Sets the usage hint.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Sets the localization key of the usage hint.
    #[must_use]
    pub fn with_hint_key(mut self, key: impl Into<String>) -> Self {
        self.hint_key = Some(key.into());
        self
    }

    /// Marks the node a live region.
    #[must_use]
    pub fn with_live(mut self, live: Politeness) -> Self {
        self.live = live;
        self
    }

    /// Overrides that hide the node and its subtree from assistive technology (decoration).
    #[must_use]
    pub fn decorative() -> Self {
        Self {
            hidden: true,
            ..Self::default()
        }
    }
}

/// The states of one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct A11yStates {
    /// Has keyboard / gamepad focus.
    pub focused: bool,
    /// Can take focus.
    pub focusable: bool,
    /// [`Disabled`].
    pub disabled: bool,
    /// [`Selected`].
    pub selected: bool,
    /// [`Invalid`].
    pub invalid: bool,
    /// A check box's state; `None` for every other role.
    pub checked: Option<bool>,
    /// A combo box's or dialog's open state; `None` for every other role.
    pub expanded: Option<bool>,
    /// Outside the top input layer: present but not reachable (a menu under an open dialog).
    pub inert: bool,
    /// A text field that shows text but takes no edits (`TextInput::read_only`).
    pub read_only: bool,
}

impl A11yStates {
    /// The names of the states that are on (`checked` and `expanded` only when `Some(true)`).
    pub fn names(&self) -> Vec<&'static str> {
        [
            (self.focused, "focused"),
            (self.focusable, "focusable"),
            (self.disabled, "disabled"),
            (self.selected, "selected"),
            (self.invalid, "invalid"),
            (self.checked == Some(true), "checked"),
            (self.expanded == Some(true), "expanded"),
            (self.inert, "inert"),
            (self.read_only, "read_only"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect()
    }
}

/// One node of the accessibility tree.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct A11yNode {
    /// The entity (`Entity::to_bits`), stable for the node's life.
    pub id: u64,
    /// The parent in the accessibility tree (flattened containers are skipped).
    pub parent: Option<u64>,
    /// Children in the accessibility tree, in order.
    pub children: Vec<u64>,
    /// The role.
    pub role: Option<A11yRole>,
    /// The accessible name ("" when none).
    pub name: String,
    /// The description ("" when none).
    pub description: String,
    /// The usage hint ("" when none), read after the description.
    pub hint: String,
    /// The value as text: a slider's number, a progress percentage, a text field's text (masked when
    /// secret), the selected option or tab.
    pub value: Option<String>,
    /// States.
    pub states: A11yStates,
    /// Bounds in physical px (x, y, width, height); zero before the first layout.
    pub bounds: [f32; 4],
    /// Live-region politeness.
    pub live: Politeness,
    /// A slider's or progress bar's number and range (`None` for every other role).
    pub range: Option<A11yRange>,
}

/// The numeric value of a slider or progress bar, for assistive technology that reads numbers
/// (the AT-SPI `Value` interface, the UI Automation range pattern).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct A11yRange {
    /// The value now.
    pub value: f64,
    /// The smallest value.
    pub min: f64,
    /// The largest value.
    pub max: f64,
    /// One step (`raise` / `lower` move by it); 0 when the widget has none.
    pub step: f64,
}

impl A11yNode {
    /// The node's role, `Group` when unset.
    pub fn role(&self) -> A11yRole {
        self.role.unwrap_or(A11yRole::Group)
    }

    /// What a screen reader would say when the node takes focus: name, role, value and the
    /// notable states, comma separated ("Volume, slider, 50"; "Subtitles, check box, checked").
    pub fn spoken_summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.name.is_empty() {
            parts.push(self.name.clone());
        }
        parts.push(self.role().spoken().to_owned());
        if let Some(value) = self.value.as_ref().filter(|v| !v.is_empty()) {
            parts.push(value.clone());
        }
        match self.states.checked {
            Some(true) => parts.push("checked".to_owned()),
            Some(false) => parts.push("not checked".to_owned()),
            None => {}
        }
        match self.states.expanded {
            Some(true) => parts.push("expanded".to_owned()),
            Some(false) if self.role() == A11yRole::ComboBox => parts.push("collapsed".to_owned()),
            _ => {}
        }
        if self.states.selected {
            parts.push("selected".to_owned());
        }
        if self.states.disabled {
            parts.push("disabled".to_owned());
        }
        if self.states.invalid {
            parts.push("invalid entry".to_owned());
        }
        if !self.description.is_empty() {
            parts.push(self.description.clone());
        }
        if !self.hint.is_empty() {
            parts.push(self.hint.clone());
        }
        parts.join(", ")
    }
}

/// A snapshot of the accessibility tree: nodes in depth-first order, roots first by sort order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct A11yTree {
    /// Every node, depth first.
    pub nodes: Vec<A11yNode>,
    /// The focused node, when it is in the tree.
    pub focus: Option<u64>,
    /// The root of the top input layer (an open dialog), when there is one.
    pub top_layer: Option<u64>,
}

/// One change between two trees, or an announcement.
#[derive(Debug, Clone, PartialEq)]
pub enum A11yEvent {
    /// A node appeared (its parent's children changed).
    Added {
        /// The node.
        id: u64,
        /// Its parent, if any.
        parent: Option<u64>,
    },
    /// A node went away.
    Removed {
        /// The node.
        id: u64,
        /// Its former parent, if any.
        parent: Option<u64>,
    },
    /// The accessible name changed.
    NameChanged {
        /// The node.
        id: u64,
    },
    /// The value changed.
    ValueChanged {
        /// The node.
        id: u64,
    },
    /// One or more states changed.
    StatesChanged {
        /// The node.
        id: u64,
        /// The states before.
        before: A11yStates,
    },
    /// Focus moved.
    FocusChanged {
        /// The node that had focus.
        from: Option<u64>,
        /// The node that has it now.
        to: Option<u64>,
    },
    /// The top input layer changed (a dialog opened or closed): a bridge moves the reader to
    /// `to`, or to the first node when no layer is left.
    ScreenChanged {
        /// The layer root before.
        from: Option<u64>,
        /// The layer root now.
        to: Option<u64>,
    },
    /// Text to speak: from [`ScreenReader::announce`] or a live region's change.
    Announcement {
        /// What to say.
        text: String,
        /// How urgently.
        politeness: Politeness,
    },
}

impl A11yEvent {
    /// The protocol name of the event kind.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Added { .. } => "added",
            Self::Removed { .. } => "removed",
            Self::NameChanged { .. } => "name_changed",
            Self::ValueChanged { .. } => "value_changed",
            Self::StatesChanged { .. } => "states_changed",
            Self::FocusChanged { .. } => "focus_changed",
            Self::ScreenChanged { .. } => "screen_changed",
            Self::Announcement { .. } => "announcement",
        }
    }
}

impl A11yTree {
    /// The node with `id`.
    pub fn get(&self, id: u64) -> Option<&A11yNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// The events that turn `previous` into `self`, in a stable order: removals; then, per node in
    /// tree order, its addition or its name, value and state changes; then live-region
    /// announcements; then the screen change (the top input layer moved); then the focus change.
    pub fn diff(&self, previous: &A11yTree) -> Vec<A11yEvent> {
        let before: HashMap<u64, &A11yNode> = previous.nodes.iter().map(|n| (n.id, n)).collect();
        let now: HashMap<u64, &A11yNode> = self.nodes.iter().map(|n| (n.id, n)).collect();
        let mut events = Vec::new();
        for old in &previous.nodes {
            if !now.contains_key(&old.id) {
                events.push(A11yEvent::Removed {
                    id: old.id,
                    parent: old.parent,
                });
            }
        }
        let mut announcements = Vec::new();
        for node in &self.nodes {
            let Some(old) = before.get(&node.id) else {
                events.push(A11yEvent::Added {
                    id: node.id,
                    parent: node.parent,
                });
                continue;
            };
            let mut spoken_change = false;
            if old.name != node.name {
                events.push(A11yEvent::NameChanged { id: node.id });
                spoken_change = true;
            }
            if old.value != node.value {
                events.push(A11yEvent::ValueChanged { id: node.id });
                spoken_change = true;
            }
            if old.states != node.states {
                events.push(A11yEvent::StatesChanged {
                    id: node.id,
                    before: old.states,
                });
            }
            if spoken_change && node.live != Politeness::Off {
                let text = match node.value.as_ref().filter(|v| !v.is_empty()) {
                    Some(value) if !node.name.is_empty() => format!("{}, {value}", node.name),
                    Some(value) => value.clone(),
                    None => node.name.clone(),
                };
                if !text.is_empty() {
                    announcements.push(A11yEvent::Announcement {
                        text,
                        politeness: node.live,
                    });
                }
            }
        }
        events.extend(announcements);
        if self.top_layer != previous.top_layer {
            events.push(A11yEvent::ScreenChanged {
                from: previous.top_layer,
                to: self.top_layer,
            });
        }
        if self.focus != previous.focus {
            events.push(A11yEvent::FocusChanged {
                from: previous.focus,
                to: self.focus,
            });
        }
        events
    }
}

fn resolved(text: &Text, loc: &Localization) -> String {
    text.spans
        .iter()
        .map(|span| match &span.content {
            TextContent::Lit(s) => s.clone(),
            content => content.resolve(loc),
        })
        .collect()
}

/// An override string: the localization `key` when the table defines it, else the `literal`, else
/// (a key nobody defined) the table's own missing-key text.
fn localized(loc: &Localization, key: Option<&str>, literal: Option<&str>) -> Option<String> {
    match (key, literal) {
        (Some(key), _) if loc.has(key) => Some(loc.resolve(key, &[])),
        (_, Some(literal)) => Some(literal.to_owned()),
        (Some(key), None) => Some(loc.resolve(key, &[])),
        (None, None) => None,
    }
}

/// A childless `Text` child: the label of its parent. A text that takes focus is a control of its own
/// (a list row), not a label, so it keeps its own node. A [`TextInput`](crate::text_input::TextInput)'s
/// label part is never one: its text mirrors the field's *value*, so folding it would speak the
/// content twice (once as the name, once as the value) and re-announce the name on every
/// keystroke; the field's placeholder names it instead.
fn is_label_child(world: &World, e: Entity) -> bool {
    world.get::<Text>(e).is_some()
        && world.get::<Children>(e).is_none_or(|k| k.is_empty())
        && world.get::<Accessible>(e).is_none()
        && world.get::<Focusable>(e).is_none()
        && world.get::<crate::text_input::TextInputPart>(e).is_none()
}

fn hidden(world: &World, e: Entity) -> bool {
    world.get::<Accessible>(e).is_some_and(|a| a.hidden)
        // A text field's label and caret parts are widget internals, not content: the field
        // itself exposes the placeholder as its name and the content as its value, so the parts
        // would only duplicate the value (and re-announce it per keystroke) in the tree.
        || world.get::<crate::text_input::TextInputPart>(e).is_some()
        || world
            .get::<NodeStyle>(e)
            .is_some_and(|s| s.0.display == Display::None)
}

fn inferred_role(world: &World, e: Entity) -> Option<A11yRole> {
    let role = if world.get::<UiRoot>(e).is_some() {
        A11yRole::Window
    } else if world.get::<Button>(e).is_some() {
        A11yRole::Button
    } else if world.get::<Toggle>(e).is_some() {
        A11yRole::CheckBox
    } else if world.get::<Slider>(e).is_some() {
        A11yRole::Slider
    } else if world.get::<ProgressBar>(e).is_some() {
        A11yRole::ProgressBar
    } else if world.get::<TextInput>(e).is_some() {
        A11yRole::TextInput
    } else if world.get::<Dropdown>(e).is_some() {
        A11yRole::ComboBox
    } else if world.get::<TabBar>(e).is_some() {
        A11yRole::TabList
    } else if world.get::<Breadcrumbs>(e).is_some() {
        A11yRole::Group
    } else if world.get::<Modal>(e).is_some() {
        A11yRole::Dialog
    } else if world.get::<RadialMenu>(e).is_some() {
        A11yRole::Menu
    } else if world.get::<ListView>(e).is_some() {
        A11yRole::List
    } else if world.get::<ScrollView>(e).is_some() {
        A11yRole::ScrollView
    } else if world.get::<Text>(e).is_some() {
        A11yRole::Label
    } else if world.get::<UiImage>(e).is_some() {
        A11yRole::Image
    } else {
        return None;
    };
    Some(role)
}

/// Formats a number without trailing zeros ("50", "0.5").
fn number_text(v: f32) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        String::new()
    }
}

fn value_of(world: &World, e: Entity) -> Option<String> {
    if let Some(slider) = world.get::<Slider>(e) {
        Some(number_text(slider.value))
    } else if let Some(bar) = world.get::<ProgressBar>(e) {
        Some(format!("{}%", (bar.value.clamp(0.0, 1.0) * 100.0).round()))
    } else if let Some(input) = world.get::<TextInput>(e) {
        Some(input.display_text())
    } else if let Some(dropdown) = world.get::<Dropdown>(e) {
        Some(
            dropdown
                .selected
                .and_then(|i| dropdown.options.get(i))
                .cloned()
                .unwrap_or_default(),
        )
    } else if let Some(bar) = world.get::<TabBar>(e) {
        Some(bar.tabs.get(bar.active).cloned().unwrap_or_default())
    } else {
        world
            .get::<Breadcrumbs>(e)
            .map(|b| b.segments.get(b.current).cloned().unwrap_or_default())
    }
}

fn range_of(world: &World, e: Entity) -> Option<A11yRange> {
    if let Some(slider) = world.get::<Slider>(e) {
        // A non-finite end or value has no numeric meaning for a screen reader, and the platform
        // adapters refuse a tree that carries one: the node exports no range (audit wave 5,
        // accesskit M4), as `number_text` blanks the text.
        if ![slider.value, slider.min, slider.max]
            .iter()
            .all(|v| v.is_finite())
        {
            return None;
        }
        let step = if slider.step.is_finite() && slider.step > 0.0 {
            slider.step
        } else {
            0.0
        };
        Some(A11yRange {
            value: f64::from(slider.value),
            min: f64::from(slider.min),
            max: f64::from(slider.max),
            step: f64::from(step),
        })
    } else {
        // The progress text is a rounded percentage; the number follows it.
        world.get::<ProgressBar>(e).map(|bar| A11yRange {
            value: f64::from((bar.value.clamp(0.0, 1.0) * 100.0).round()),
            min: 0.0,
            max: 100.0,
            step: 0.0,
        })
    }
}

/// Builds the accessibility tree of every [`UiRoot`] (roots by `sort_order`, then entity).
///
/// Per node: the role is [`Accessible::role`] or inferred from the widget; the name is
/// [`Accessible::label`], else the node's own [`Text`], else its label children's text joined by
/// spaces, else a text field's placeholder; label children are folded into their parent. An
/// unnamed container with no role, no [`Accessible`] and no focus is flattened. Hidden subtrees
/// (`Display::None`, [`Accessible::hidden`]) are skipped. Nodes outside the top input layer are
/// `inert`. The walk is bounded: [`MAX_A11Y_DEPTH`] levels and [`MAX_A11Y_NODES`] nodes.
pub fn build_a11y_tree(world: &mut World) -> A11yTree {
    #[cfg(test)]
    A11Y_TREE_BUILDS.with(|n| n.set(n.get().wrapping_add(1)));
    let _span = tracing::trace_span!("ui_a11y_build").entered();
    let mut roots: Vec<(i32, Entity)> = world
        .query::<(Entity, &UiRoot)>()
        .iter(world)
        .map(|(e, r)| (r.sort_order, e))
        .collect();
    roots.sort_by_key(|(order, e)| (*order, e.to_bits()));
    let fallback = Localization::default();
    let world: &World = world;
    let loc = world.get_resource::<Localization>().unwrap_or(&fallback);
    let top = world
        .get_resource::<crate::layers::InputLayers>()
        .and_then(crate::layers::InputLayers::top_root);
    let focus_entity = world
        .get_resource::<crate::focus::FocusState>()
        .and_then(|f| f.focused);
    let mut tree = A11yTree::default();
    let mut seen = std::collections::HashSet::new();
    let mut cut = false;
    for (_, root) in roots {
        visit(world, loc, top, root, None, &mut tree, &mut seen, 0, &mut cut);
    }
    if cut {
        tracing::warn!(
            max_depth = MAX_A11Y_DEPTH,
            max_nodes = MAX_A11Y_NODES,
            "the UI hierarchy nests deeper or holds more nodes than the accessibility tree walks; the rest is left out"
        );
    }
    tree.focus = focus_entity
        .map(Entity::to_bits)
        .filter(|id| tree.nodes.iter().any(|n| n.id == *id));
    tree.top_layer = top.map(Entity::to_bits);
    tree
}

/// How many hierarchy levels (flattened ones included) the accessibility walk descends; deeper
/// entities are left out of the tree, so a pathologically deep hierarchy cannot overflow the
/// stack (audit wave 5, accesskit H1). Four times the `.ui.ron` nesting limit.
pub const MAX_A11Y_DEPTH: usize = 512;

/// The most nodes the accessibility tree holds; the walk stops there (depth first, so the first
/// nodes in reading order are kept) and sets the same cut flag as [`MAX_A11Y_DEPTH`]. While a
/// screen reader is live every frame rebuilds, diffs and copies the tree, so a UI of hundreds of
/// thousands of nodes must not reach the platform whole (audit accesskit M1).
pub const MAX_A11Y_NODES: usize = 65_536;

/// Adds `e` (or, when it is flattened, its children) under `parent`. `seen` holds every entity
/// visited so far: hierarchies are trees, and a corrupt one (a `Children` cycle) is walked once
/// instead of recursing until the stack overflows (audit-seq Low). `depth` is the recursion
/// depth; at [`MAX_A11Y_DEPTH`], or once the tree holds [`MAX_A11Y_NODES`], the walk stops and
/// sets `cut`.
fn visit(
    world: &World,
    loc: &Localization,
    top: Option<Entity>,
    e: Entity,
    parent: Option<usize>,
    tree: &mut A11yTree,
    seen: &mut std::collections::HashSet<Entity>,
    depth: usize,
    cut: &mut bool,
) {
    if depth >= MAX_A11Y_DEPTH || tree.nodes.len() >= MAX_A11Y_NODES {
        *cut = true;
        return;
    }
    if !seen.insert(e) || hidden(world, e) {
        return;
    }
    let overrides = world.get::<Accessible>(e);
    let role = overrides.and_then(|a| a.role).or_else(|| inferred_role(world, e));
    let children: Vec<Entity> = world
        .get::<Children>(e)
        .map(|c| c.iter().collect())
        .unwrap_or_default();
    // Label children fold into the name of a node that has no text of its own: one with a role, or
    // one that takes focus (a tab, a custom control). Empty text counts as none: a recycled list row
    // keeps an empty `Text` and shows its label in children.
    let own_text = world
        .get::<Text>(e)
        .map(|t| resolved(t, loc))
        .filter(|t| !t.trim().is_empty());
    let focusable = world.get::<Focusable>(e).is_some();
    let labels: Vec<Entity> =
        if own_text.is_none() && (focusable || role.is_some_and(|r| r != A11yRole::Window)) {
            children
                .iter()
                .copied()
                .filter(|c| is_label_child(world, *c) && !hidden(world, *c))
                .collect()
        } else {
            Vec::new()
        };
    let label_text = (!labels.is_empty()).then(|| {
        labels
            .iter()
            .filter_map(|c| world.get::<Text>(*c).map(|t| resolved(t, loc)))
            .collect::<Vec<_>>()
            .join(" ")
    });
    let name = overrides
        .and_then(|a| localized(loc, a.label_key.as_deref(), a.label.as_deref()))
        .or(own_text)
        .or(label_text)
        .or_else(|| {
            world
                .get::<TextInput>(e)
                .map(|i| i.placeholder.clone())
                .filter(|p| !p.is_empty())
        })
        .unwrap_or_default();
    let flatten = role.is_none() && overrides.is_none() && !focusable && name.is_empty();
    if flatten {
        for child in children {
            visit(world, loc, top, child, parent, tree, seen, depth + 1, cut);
        }
        return;
    }
    let id = e.to_bits();
    let states = A11yStates {
        focused: world.get::<Focused>(e).is_some(),
        focusable,
        disabled: crate::node::is_disabled(world, e),
        selected: world.get::<Selected>(e).is_some(),
        invalid: world.get::<Invalid>(e).is_some(),
        checked: world.get::<Toggle>(e).map(|t| t.checked),
        expanded: world
            .get::<Dropdown>(e)
            .map(|d| d.open)
            .or_else(|| world.get::<Modal>(e).map(|m| m.open)),
        inert: top.is_some_and(|root| !crate::layers::within(world, root, e)),
        read_only: world.get::<TextInput>(e).is_some_and(|i| i.read_only),
    };
    let bounds = world.get::<ComputedNode>(e).map_or([0.0; 4], |c| c.rect);
    let index = tree.nodes.len();
    let parent_id = parent.map(|p| tree.nodes[p].id);
    tree.nodes.push(A11yNode {
        id,
        parent: parent_id,
        children: Vec::new(),
        role: Some(role.unwrap_or(A11yRole::Group)),
        name,
        description: overrides
            .and_then(|a| localized(loc, a.description_key.as_deref(), a.description.as_deref()))
            .unwrap_or_default(),
        hint: overrides
            .and_then(|a| localized(loc, a.hint_key.as_deref(), a.hint.as_deref()))
            .unwrap_or_default(),
        value: value_of(world, e),
        states,
        bounds,
        live: overrides.map_or(Politeness::Off, |a| a.live),
        range: range_of(world, e),
    });
    if let Some(p) = parent {
        tree.nodes[p].children.push(id);
    }
    for child in children {
        if labels.contains(&child) {
            continue;
        }
        visit(world, loc, top, child, Some(index), tree, seen, depth + 1, cut);
    }
}

/// A direction to page a scroll view in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PageDirection {
    /// Toward the start of the content, vertically.
    Up,
    /// Toward the end of the content, vertically.
    Down,
    /// Toward the start of the content, horizontally.
    Left,
    /// Toward the end of the content, horizontally.
    Right,
}

/// What a reader asks a node to do.
#[derive(Debug, Clone, PartialEq)]
pub enum A11yAction {
    /// Activate: click a button, flip a check box, open a combo box; focus a text field.
    Press,
    /// Step a slider up by its own step (a tenth of its range when it has none).
    Raise,
    /// Step a slider down.
    Lower,
    /// Replace a text field's text, held to what typing it would allow: control characters are
    /// stripped, `max_chars` cuts on grapheme boundaries, the field's filter can refuse it, and a
    /// text past [`MAX_SET_TEXT_CHARS`] is refused.
    SetText(String),
    /// Move focus to the node.
    Focus,
    /// Dismiss the dialog the node is, or is inside.
    Close,
    /// Scroll the enclosing scroll view until the node shows.
    ScrollIntoView,
    /// Scroll the node's scroll view (or the enclosing one) by 90% of its size.
    Page(PageDirection),
}

impl A11yAction {
    /// The protocol names of every action, in declaration order.
    pub const NAMES: [&'static str; 11] = [
        "press",
        "raise",
        "lower",
        "set_text",
        "focus",
        "close",
        "scroll_into_view",
        "page_up",
        "page_down",
        "page_left",
        "page_right",
    ];

    /// The protocol name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Press => "press",
            Self::Raise => "raise",
            Self::Lower => "lower",
            Self::SetText(_) => "set_text",
            Self::Focus => "focus",
            Self::Close => "close",
            Self::ScrollIntoView => "scroll_into_view",
            Self::Page(PageDirection::Up) => "page_up",
            Self::Page(PageDirection::Down) => "page_down",
            Self::Page(PageDirection::Left) => "page_left",
            Self::Page(PageDirection::Right) => "page_right",
        }
    }

    /// Parses [`Self::name`]; `set_text` takes its text from `value` (`None` when it is missing).
    pub fn parse(name: &str, value: Option<&str>) -> Option<Self> {
        Some(match name {
            "press" => Self::Press,
            "raise" => Self::Raise,
            "lower" => Self::Lower,
            "set_text" => Self::SetText(value?.to_owned()),
            "focus" => Self::Focus,
            "close" => Self::Close,
            "scroll_into_view" => Self::ScrollIntoView,
            "page_up" => Self::Page(PageDirection::Up),
            "page_down" => Self::Page(PageDirection::Down),
            "page_left" => Self::Page(PageDirection::Left),
            "page_right" => Self::Page(PageDirection::Right),
            _ => return None,
        })
    }
}

/// One request from assistive technology: `action` on the node `node` (`Entity::to_bits`).
#[derive(Debug, Clone, PartialEq)]
pub struct A11yRequest {
    /// The target node id, as the tree reports it.
    pub node: u64,
    /// What to do.
    pub action: A11yAction,
}

/// Why a request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum A11yReject {
    /// The node does not exist or is not in the accessibility tree.
    UnknownNode,
    /// The node is disabled (a disabled control never acts, as with a click).
    Disabled,
    /// The node is outside the top input layer (under an open dialog).
    Inert,
    /// The node cannot do this action (raise on a button, set text on a read-only field).
    Unsupported,
    /// The dialog has `close_on_cancel` off.
    NotDismissable,
    /// The node is not in a scroll view.
    NoScrollView,
}

impl A11yReject {
    /// The protocol reason (`snake_case`, documented in `book/agents/ui-a11y.md`).
    pub fn reason(self) -> &'static str {
        match self {
            Self::UnknownNode => "unknown_node",
            Self::Disabled => "node_disabled",
            Self::Inert => "node_inert",
            Self::Unsupported => "unsupported_action",
            Self::NotDismissable => "not_dismissable",
            Self::NoScrollView => "no_scroll_view",
        }
    }
}

/// How much of a scroll view's size one page moves.
const PAGE_FRACTION: f32 = 0.9;

/// The longest text (in characters) an [`A11yAction::SetText`] request may carry; a longer one is
/// refused as `unsupported_action`. The same cap as the agent's `ui.a11y_action` value.
pub const MAX_SET_TEXT_CHARS: usize = 4096;

fn ancestors_or_self(world: &World, start: Entity) -> impl Iterator<Item = Entity> + '_ {
    // Hierarchies are trees; the bound only guards against a corrupt one.
    std::iter::successors(Some(start), |e| world.get::<ChildOf>(*e).map(|p| p.0)).take(4096)
}

/// Checks `request` against a fresh tree and runs it down the path a key press, click or scroll
/// takes: a `Click` for [`A11yAction::Press`] and [`A11yAction::Close`] (the modal closes the way
/// `Cancel` closes it), a `ValueChanged` for slider steps and text, the focus move of the
/// keyboard, and the scroll chain of a wheel. The widgets react in the same update, so the effect
/// shows after the next frame. Refusals leave the world untouched.
pub fn apply_request(world: &mut World, request: &A11yRequest) -> Result<(), A11yReject> {
    let tree = build_a11y_tree(world);
    apply_request_in(world, &tree, request)
}

/// [`apply_request`] against a tree the caller built this frame: [`route_a11y_requests`] builds
/// one tree per frame for every request it drains instead of one per request (audit wave 5,
/// accesskit H2). A request's effects land as messages and value writes that the checks below
/// (disabled, inert, role, focusable) do not read, so one snapshot answers like a fresh one.
fn apply_request_in(world: &mut World, tree: &A11yTree, request: &A11yRequest) -> Result<(), A11yReject> {
    let _span = tracing::trace_span!("ui_a11y_request").entered();
    let entity = Entity::try_from_bits(request.node)
        .filter(|e| world.get_entity(*e).is_ok())
        .ok_or(A11yReject::UnknownNode)?;
    let node = tree.get(request.node).ok_or(A11yReject::UnknownNode)?;
    if node.states.disabled {
        return Err(A11yReject::Disabled);
    }
    if node.states.inert {
        return Err(A11yReject::Inert);
    }
    let (role, focusable) = (node.role(), node.states.focusable);
    match &request.action {
        A11yAction::Press => match role {
            A11yRole::Slider => Err(A11yReject::Unsupported),
            A11yRole::TextInput => focus_node(world, entity, focusable),
            A11yRole::Button | A11yRole::CheckBox | A11yRole::ComboBox => click(world, entity),
            _ if focusable => click(world, entity),
            _ => Err(A11yReject::Unsupported),
        },
        A11yAction::Raise => step_slider(world, entity, 1.0),
        A11yAction::Lower => step_slider(world, entity, -1.0),
        A11yAction::SetText(text) => {
            // The text goes in the way typing it would: control characters stripped, the
            // field's filter and grapheme-safe `max_chars` applied, and a length cap.
            if text.chars().nth(MAX_SET_TEXT_CHARS).is_some() {
                return Err(A11yReject::Unsupported);
            }
            let clean = world
                .get::<TextInput>(entity)
                .filter(|i| !i.read_only)
                .and_then(|i| i.external_text(text))
                .ok_or(A11yReject::Unsupported)?;
            crate::agent_bridge::apply_value(world, entity, crate::interaction::UiValue::Text(clean));
            Ok(())
        }
        A11yAction::Focus => focus_node(world, entity, focusable),
        A11yAction::Close => {
            let modal = ancestors_or_self(world, entity)
                .find(|e| world.get::<Modal>(*e).is_some())
                .ok_or(A11yReject::Unsupported)?;
            let close_on_cancel = world.get::<Modal>(modal).is_some_and(|m| m.close_on_cancel);
            if !close_on_cancel {
                return Err(A11yReject::NotDismissable);
            }
            click(world, modal)
        }
        A11yAction::ScrollIntoView => {
            let view = ancestors_or_self(world, entity)
                .skip(1)
                .find(|e| world.get::<ScrollView>(*e).is_some())
                .ok_or(A11yReject::NoScrollView)?;
            let view_rect = world.get::<ComputedNode>(view).map_or([0.0; 4], |c| c.rect);
            let node_rect = world.get::<ComputedNode>(entity).map_or([0.0; 4], |c| c.rect);
            let shift = crate::scroll::focus_shift(view_rect, node_rect);
            crate::scroll::scroll_by(world, view, shift);
            Ok(())
        }
        A11yAction::Page(direction) => {
            let view = ancestors_or_self(world, entity)
                .find(|e| world.get::<ScrollView>(*e).is_some())
                .ok_or(A11yReject::NoScrollView)?;
            let rect = world.get::<ComputedNode>(view).map_or([0.0; 4], |c| c.rect);
            let (w, h) = (rect[2] * PAGE_FRACTION, rect[3] * PAGE_FRACTION);
            let delta = match direction {
                PageDirection::Up => [0.0, -h],
                PageDirection::Down => [0.0, h],
                PageDirection::Left => [-w, 0.0],
                PageDirection::Right => [w, 0.0],
            };
            crate::scroll::scroll_by(world, view, delta);
            Ok(())
        }
    }
}

fn click(world: &mut World, entity: Entity) -> Result<(), A11yReject> {
    world
        .write_message(UiEvent::new(entity, UiEventKind::Click))
        .map(|_| ())
        .ok_or(A11yReject::Unsupported)
}

fn focus_node(world: &mut World, entity: Entity, focusable: bool) -> Result<(), A11yReject> {
    if !focusable || !world.contains_resource::<crate::focus::FocusState>() {
        return Err(A11yReject::Unsupported);
    }
    crate::agent_bridge::apply_focus(world, entity);
    Ok(())
}

fn step_slider(world: &mut World, entity: Entity, direction: f32) -> Result<(), A11yReject> {
    let next = {
        let slider = world.get::<Slider>(entity).ok_or(A11yReject::Unsupported)?;
        let span = (slider.max - slider.min).abs();
        let step = if slider.step.is_finite() && slider.step > 0.0 {
            slider.step
        } else {
            span / 10.0
        };
        slider.value + direction * step
    };
    crate::agent_bridge::apply_value(world, entity, crate::interaction::UiValue::Number(next));
    Ok(())
}

/// The most requests a [`RequestQueue`] holds; pushing past it drops the oldest (audit wave 5,
/// accesskit H2 and M5). Four times the OS bridge's inbound queue.
pub const MAX_QUEUED_REQUESTS: usize = 256;

/// A queue a test or a bridge thread fills while the world drains it: the shared half of a
/// request-capable backend. It holds at most [`MAX_QUEUED_REQUESTS`].
#[derive(Debug, Clone, Default)]
pub struct RequestQueue(Arc<Mutex<VecDeque<A11yRequest>>>);

impl RequestQueue {
    /// Queues `request` for the next frame; when the queue is full the oldest request is dropped.
    pub fn push(&self, request: A11yRequest) {
        if let Ok(mut queue) = self.0.lock() {
            if queue.len() >= MAX_QUEUED_REQUESTS {
                queue.pop_front();
                tracing::warn!(
                    cap = MAX_QUEUED_REQUESTS,
                    "a11y request queue full; dropped the oldest request"
                );
            }
            queue.push_back(request);
        }
    }

    /// Takes everything queued, oldest first.
    pub fn take(&self) -> Vec<A11yRequest> {
        self.0
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default()
    }
}

/// A headless backend: always active, records every event the tree sync sends and every request
/// result, and takes requests through its [`RecordingHandle`]. For tests and agents.
#[derive(Debug)]
pub struct RecordingReader {
    requests: RequestQueue,
    log: Arc<Mutex<RecordingLog>>,
}

#[derive(Debug, Default)]
struct RecordingLog {
    events: Vec<A11yEvent>,
    results: Vec<(A11yRequest, Result<(), A11yReject>)>,
}

/// The test side of a [`RecordingReader`] (the reader itself moves into the world).
#[derive(Debug, Clone)]
pub struct RecordingHandle {
    requests: RequestQueue,
    log: Arc<Mutex<RecordingLog>>,
}

impl RecordingReader {
    /// A reader and the handle that feeds and inspects it.
    pub fn new() -> (Self, RecordingHandle) {
        let requests = RequestQueue::default();
        let log = Arc::new(Mutex::new(RecordingLog::default()));
        (
            Self {
                requests: requests.clone(),
                log: Arc::clone(&log),
            },
            RecordingHandle { requests, log },
        )
    }
}

impl RecordingHandle {
    /// Queues a request for the next frame.
    pub fn request(&self, node: u64, action: A11yAction) {
        self.requests.push(A11yRequest { node, action });
    }

    /// Every event the sync sent, oldest first.
    pub fn events(&self) -> Vec<A11yEvent> {
        self.log.lock().map(|l| l.events.clone()).unwrap_or_default()
    }

    /// Forgets the recorded events (not the results).
    pub fn clear_events(&self) {
        if let Ok(mut log) = self.log.lock() {
            log.events.clear();
        }
    }

    /// Every request that was routed and how it ended, oldest first.
    pub fn results(&self) -> Vec<(A11yRequest, Result<(), A11yReject>)> {
        self.log.lock().map(|l| l.results.clone()).unwrap_or_default()
    }
}

impl ScreenReaderBackend for RecordingReader {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn is_active(&self) -> bool {
        true
    }

    fn tree_updated(&mut self, _tree: &A11yTree, events: &[A11yEvent]) {
        if let Ok(mut log) = self.log.lock() {
            log.events.extend(events.iter().cloned());
        }
    }

    fn drain_requests(&mut self) -> Vec<A11yRequest> {
        self.requests.take()
    }

    fn request_result(&mut self, request: &A11yRequest, result: Result<(), A11yReject>) {
        if let Ok(mut log) = self.log.lock() {
            log.results.push((request.clone(), result));
        }
    }
}

/// Where accessibility updates go: a platform bridge (AT-SPI, UI Automation, Android), a
/// text-to-speech voice, or a recorder in tests.
pub trait ScreenReaderBackend: Send + Sync + 'static {
    /// A short name for logs and the agent ("atspi_stub", "none").
    fn name(&self) -> &'static str;
    /// Whether assistive technology is listening. While it is not (and exporting is off), the tree
    /// is not rebuilt.
    fn is_active(&self) -> bool;
    /// The tree changed: `tree` is the new snapshot, `events` what changed since the last call
    /// (the first call after activation reports every node as added).
    fn tree_updated(&mut self, tree: &A11yTree, events: &[A11yEvent]);
    /// Recent backend-level events as text, newest last (for the agent and tests); empty by default.
    fn recent_events(&self) -> Vec<String> {
        Vec::new()
    }
    /// Requests assistive technology made since the last call (press, raise, ...), oldest first.
    /// [`route_a11y_requests`] calls it each frame while the reader is live; empty by default.
    fn drain_requests(&mut self) -> Vec<A11yRequest> {
        Vec::new()
    }
    /// How a drained request ended, so the bridge can answer the platform call; does nothing by
    /// default.
    fn request_result(&mut self, _request: &A11yRequest, _result: Result<(), A11yReject>) {}
    /// Whether the backend wants [`Self::tree_updated`] this frame even with no event (it owes a
    /// full tree to an assistive technology that just connected, or has deferred changes left);
    /// `false` by default.
    fn wants_sync(&self) -> bool {
        false
    }
}

/// The backend that does nothing (never active).
#[derive(Debug, Default)]
pub struct NullScreenReader;

impl ScreenReaderBackend for NullScreenReader {
    fn name(&self) -> &'static str {
        "none"
    }

    fn is_active(&self) -> bool {
        false
    }

    fn tree_updated(&mut self, _tree: &A11yTree, _events: &[A11yEvent]) {}
}

/// The platform's default backend: the AT-SPI stub on Linux (active when
/// `REGIA_SCREEN_READER=1`), nothing elsewhere. With the `accesskit` feature,
/// [`ScreenReaderPlugin`] installs the AccessKit bridge instead.
pub fn default_backend() -> Box<dyn ScreenReaderBackend> {
    if cfg!(target_os = "linux") {
        Box::new(atspi::AtSpiStub::detect())
    } else {
        Box::new(NullScreenReader)
    }
}

/// How many recent announcements [`ScreenReader`] keeps for the agent.
const RECENT_ANNOUNCEMENTS: usize = 32;

/// Live syncs [`SyncCost`] keeps (about 8 s at 60 fps).
pub const SYNC_COST_WINDOW: usize = 512;

/// What [`sync_screen_reader`] cost on its last [`SYNC_COST_WINDOW`] live frames (ADR-5379): the
/// per-frame price of a connected screen reader, measured where it runs (`ui.a11y_tree` reports it).
/// Idle frames (no reader) are not recorded; they cost one resource lookup.
#[derive(Debug, Clone, Default)]
pub struct SyncCost {
    /// Nanoseconds per live sync, oldest first: the whole sync, then its tree build, its diff and
    /// the backend's update ([`SyncStage`] order after the total).
    samples: VecDeque<[u64; 4]>,
    /// Live syncs ever recorded.
    total: u64,
}

/// A part of one screen-reader sync ([`SyncCost::stage_mean_ms`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStage {
    /// [`build_a11y_tree`].
    Build,
    /// [`A11yTree::diff`] against the last tree.
    Diff,
    /// [`ScreenReaderBackend::tree_updated`] (the OS bridge's own diff and hand-off).
    Backend,
}

impl SyncStage {
    /// Every stage, in sample order.
    pub const ALL: [Self; 3] = [Self::Build, Self::Diff, Self::Backend];

    /// The protocol name (`build`, `diff`, `backend`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Diff => "diff",
            Self::Backend => "backend",
        }
    }
}

fn nanos(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl SyncCost {
    /// Records one live sync that took `cost` (no stage split); the oldest falls out past
    /// [`SYNC_COST_WINDOW`].
    pub fn record(&mut self, cost: std::time::Duration) {
        self.push([nanos(cost), 0, 0, 0]);
    }

    /// Records one live sync that took `total`, of which `stages` (in [`SyncStage::ALL`] order).
    pub fn record_stages(&mut self, total: std::time::Duration, stages: [std::time::Duration; 3]) {
        self.push([nanos(total), nanos(stages[0]), nanos(stages[1]), nanos(stages[2])]);
    }

    fn push(&mut self, sample: [u64; 4]) {
        if self.samples.len() == SYNC_COST_WINDOW {
            self.samples.pop_front();
        } else if self.samples.capacity() == 0 {
            self.samples.reserve_exact(SYNC_COST_WINDOW);
        }
        self.samples.push_back(sample);
        self.total += 1;
    }

    /// Mean cost of `stage` over the window in ms (0 when empty or not split).
    pub fn stage_mean_ms(&self, stage: SyncStage) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let column = 1 + stage as usize;
        self.samples.iter().map(|s| s[column] as f64).sum::<f64>() / self.samples.len() as f64 / 1e6
    }

    /// Live syncs in the window.
    pub fn samples(&self) -> usize {
        self.samples.len()
    }

    /// Live syncs ever recorded.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Mean cost over the window in ms (0 when empty).
    pub fn mean_ms(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.samples.iter().map(|s| s[0] as f64).sum::<f64>() / self.samples.len() as f64 / 1e6
    }

    /// The nearest-rank `percent` percentile over the window in ms (0 when empty): the smallest
    /// sample that at least `percent` % of the samples do not exceed.
    pub fn percentile_ms(&self, percent: f64) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<u64> = self.samples.iter().map(|s| s[0]).collect();
        sorted.sort_unstable();
        let rank = (percent.clamp(0.0, 100.0) / 100.0 * sorted.len() as f64).ceil() as usize;
        sorted[rank.clamp(1, sorted.len()) - 1] as f64 / 1e6
    }

    /// The largest cost in the window in ms (0 when empty).
    pub fn max_ms(&self) -> f64 {
        self.samples.iter().map(|s| s[0]).max().unwrap_or(0) as f64 / 1e6
    }
}

/// The clock [`sync_screen_reader`] times itself with; none on the web, where `std::time::Instant`
/// is unavailable.
fn sync_clock() -> Option<std::time::Instant> {
    #[cfg(target_arch = "wasm32")]
    {
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Some(std::time::Instant::now())
    }
}

/// The most announcements queued for one sync; [`ScreenReader::announce`] drops the oldest past
/// it, so a loop announcing every frame cannot grow the queue or the joined announcer label without
/// bound (audit wave 5, accesskit M5).
pub const MAX_PENDING_ANNOUNCEMENTS: usize = 64;

/// The screen-reader hook: the backend, the last exported tree and pending announcements.
#[derive(Resource)]
pub struct ScreenReader {
    backend: Box<dyn ScreenReaderBackend>,
    tree: A11yTree,
    exporting: bool,
    pending: Vec<(String, Politeness)>,
    recent: VecDeque<(String, Politeness)>,
    requests: VecDeque<(A11yRequest, Result<(), A11yReject>)>,
    was_live: bool,
    /// Frames the tree was rebuilt (a counter for tests and the agent).
    pub builds: u64,
    cost: SyncCost,
    removals: RemovalWatch,
    /// The world's change tick when `tree` was built: [`route_a11y_requests`] answers from the
    /// stored tree only while nothing the tree reads changed since (ADR-5567).
    tree_tick: Tick,
}

impl Default for ScreenReader {
    fn default() -> Self {
        Self::with_backend(default_backend())
    }
}

impl ScreenReader {
    /// A screen-reader hook over `backend`.
    pub fn with_backend(backend: Box<dyn ScreenReaderBackend>) -> Self {
        Self {
            backend,
            tree: A11yTree::default(),
            exporting: false,
            pending: Vec::new(),
            recent: VecDeque::new(),
            requests: VecDeque::new(),
            was_live: false,
            builds: 0,
            cost: SyncCost::default(),
            removals: RemovalWatch::default(),
            tree_tick: Tick::new(0),
        }
    }

    /// What the live syncs cost recently ([`SyncCost`]).
    pub fn sync_cost(&self) -> &SyncCost {
        &self.cost
    }

    /// The backend.
    pub fn backend(&self) -> &dyn ScreenReaderBackend {
        self.backend.as_ref()
    }

    /// Replaces the backend; the next sync reports the whole tree to it.
    pub fn set_backend(&mut self, backend: Box<dyn ScreenReaderBackend>) {
        self.backend = backend;
        self.tree = A11yTree::default();
        self.was_live = false;
    }

    /// Keeps the tree exported every frame even with no assistive technology listening (agents,
    /// tests, a host's own voice).
    pub fn set_exporting(&mut self, on: bool) {
        self.exporting = on;
    }

    /// Whether the tree is exported every frame: exporting is on, or the backend is active.
    pub fn is_live(&self) -> bool {
        self.exporting || self.backend.is_active()
    }

    /// The last exported tree (empty until the first sync while live).
    pub fn tree(&self) -> &A11yTree {
        &self.tree
    }

    /// Queues `text` to be spoken; it reaches the backend on the next sync. At most
    /// [`MAX_PENDING_ANNOUNCEMENTS`] wait for one sync: past that the oldest is dropped.
    pub fn announce(&mut self, text: impl Into<String>, politeness: Politeness) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        if self.pending.len() >= MAX_PENDING_ANNOUNCEMENTS {
            self.pending.remove(0);
            tracing::warn!(
                cap = MAX_PENDING_ANNOUNCEMENTS,
                "too many announcements in one frame; dropped the oldest"
            );
        }
        self.pending.push((text, politeness));
    }

    /// The most recent announcements (queued or from live regions), oldest first.
    pub fn recent_announcements(&self) -> impl Iterator<Item = (&str, Politeness)> {
        self.recent.iter().map(|(t, p)| (t.as_str(), *p))
    }

    /// The most recent requests that were routed and how each ended, oldest first.
    pub fn recent_requests(&self) -> impl Iterator<Item = &(A11yRequest, Result<(), A11yReject>)> {
        self.requests.iter()
    }

    /// Tells the backend how a request it drained ended, and remembers it.
    pub fn record_request(&mut self, request: A11yRequest, result: Result<(), A11yReject>) {
        self.backend.request_result(&request, result);
        self.note_request(request, result);
    }

    /// Remembers a request that did not come from the backend (the agent's `ui.a11y_act`).
    pub fn note_request(&mut self, request: A11yRequest, result: Result<(), A11yReject>) {
        if self.requests.len() == RECENT_ANNOUNCEMENTS {
            self.requests.pop_front();
        }
        self.requests.push_back((request, result));
    }

    fn remember(&mut self, text: &str, politeness: Politeness) {
        if self.recent.len() == RECENT_ANNOUNCEMENTS {
            self.recent.pop_front();
        }
        self.recent.push_back((text.to_owned(), politeness));
    }
}

/// Widgets, focus, the input layer or the locale changed since the last tracker clear.
fn a11y_inputs_dirty(world: &mut World) -> bool {
    let locale = world
        .get_resource_ref::<Localization>()
        .is_some_and(|loc| loc.is_changed());
    let focus = world
        .get_resource_ref::<crate::focus::FocusState>()
        .is_some_and(|focus| focus.is_changed());
    let layers = world
        .get_resource_ref::<crate::layers::InputLayers>()
        .is_some_and(|layers| layers.is_changed());
    let widgets = world
        .query_filtered::<Entity, Or<(
            Or<(
                Changed<Text>,
                Changed<Accessible>,
                Changed<UiRoot>,
                Changed<Children>,
                Changed<ComputedNode>,
                Changed<NodeStyle>,
                Changed<Button>,
                Changed<Toggle>,
                Changed<Slider>,
                Changed<ProgressBar>,
                Changed<TextInput>,
                Changed<Dropdown>,
            )>,
            Or<(
                Changed<crate::widgets::TabBar>,
                Changed<crate::default_widgets::Breadcrumbs>,
                Changed<Modal>,
                Changed<crate::radial::RadialMenu>,
                Changed<crate::scroll::ListView>,
                Changed<crate::scroll::ScrollView>,
                Changed<Focusable>,
                Changed<Focused>,
                Changed<Disabled>,
                Changed<crate::node::DisabledSubtree>,
                Changed<Selected>,
                Changed<Invalid>,
                Changed<UiImage>,
            )>,
        )>>()
        .iter(world)
        .next()
        .is_some();
    #[cfg(test)]
    {
        let mut reason = String::new();
        if locale {
            reason.push_str("locale ");
        }
        if focus {
            reason.push_str("focus ");
        }
        if layers {
            reason.push_str("layers ");
        }
        if widgets {
            reason.push_str("widgets: ");
            reason.push_str(&changed_a11y_components(world));
        }
        if let Ok(mut slot) = A11Y_DIRTY_REASON.lock() {
            *slot = reason;
        }
    }
    locale || focus || layers || widgets
}

/// Read positions in the removal queues of the components the tree reads.
///
/// `Changed` never fires for a removed component or a despawned node, so without this a
/// re-enabled button stayed "disabled" and a closed menu stayed in the reader's tree. Each
/// cursor reads both buffers of its queue, so a removal made after the last sync (later in that
/// frame, before the tracker clear) still counts at the next one.
#[derive(Default)]
struct RemovalWatch {
    cursors: Vec<(ComponentId, MessageCursor<RemovedComponentEntity>)>,
}

impl RemovalWatch {
    /// Whether a watched component was removed (or its entity despawned) since the last poll.
    fn poll(&mut self, world: &World) -> bool {
        let ids = [
            world.component_id::<crate::node::UiNode>(),
            world.component_id::<ChildOf>(),
            world.component_id::<Children>(),
            world.component_id::<Text>(),
            world.component_id::<Accessible>(),
            world.component_id::<UiRoot>(),
            world.component_id::<NodeStyle>(),
            world.component_id::<Button>(),
            world.component_id::<Toggle>(),
            world.component_id::<Slider>(),
            world.component_id::<ProgressBar>(),
            world.component_id::<TextInput>(),
            world.component_id::<Dropdown>(),
            world.component_id::<TabBar>(),
            world.component_id::<Breadcrumbs>(),
            world.component_id::<Modal>(),
            world.component_id::<RadialMenu>(),
            world.component_id::<ListView>(),
            world.component_id::<ScrollView>(),
            world.component_id::<Focusable>(),
            world.component_id::<Focused>(),
            world.component_id::<Disabled>(),
            world.component_id::<crate::node::DisabledSubtree>(),
            world.component_id::<Selected>(),
            world.component_id::<Invalid>(),
            world.component_id::<UiImage>(),
        ];
        let queues = world.removed_components();
        let mut removed = false;
        for id in ids.into_iter().flatten() {
            let Some(messages) = queues.get(id) else {
                continue;
            };
            let slot = match self.cursors.iter().position(|(c, _)| *c == id) {
                Some(slot) => slot,
                None => {
                    self.cursors.push((id, MessageCursor::default()));
                    self.cursors.len() - 1
                }
            };
            let cursor = &mut self.cursors[slot].1;
            removed |= !cursor.is_empty(messages);
            cursor.clear(messages);
        }
        removed
    }
}

/// Names of watched components that changed, for the quiet-frame test.
#[cfg(test)]
fn changed_a11y_components(world: &mut World) -> String {
    let mut names = String::new();
    macro_rules! note {
        ($ty:ty, $name:literal) => {
            let n = world
                .query_filtered::<Entity, Changed<$ty>>()
                .iter(world)
                .count();
            if n > 0 {
                if !names.is_empty() {
                    names.push_str(", ");
                }
                names.push_str($name);
                names.push(' ');
                names.push_str(&n.to_string());
            }
        };
    }
    note!(Text, "Text");
    note!(Accessible, "Accessible");
    note!(UiRoot, "UiRoot");
    note!(Children, "Children");
    note!(ComputedNode, "ComputedNode");
    note!(NodeStyle, "NodeStyle");
    note!(Button, "Button");
    note!(Toggle, "Toggle");
    note!(Slider, "Slider");
    note!(ProgressBar, "ProgressBar");
    note!(TextInput, "TextInput");
    note!(Dropdown, "Dropdown");
    note!(crate::widgets::TabBar, "TabBar");
    note!(crate::default_widgets::Breadcrumbs, "Breadcrumbs");
    note!(Modal, "Modal");
    note!(crate::radial::RadialMenu, "RadialMenu");
    note!(crate::scroll::ListView, "ListView");
    note!(crate::scroll::ScrollView, "ScrollView");
    note!(Focusable, "Focusable");
    note!(Focused, "Focused");
    note!(Disabled, "Disabled");
    note!(Selected, "Selected");
    note!(Invalid, "Invalid");
    note!(UiImage, "UiImage");
    names
}

/// `PostUpdate`, after text shaping: while the screen reader is live, rebuild the tree when it
/// just became live or the widgets changed, diff it against the last one and hand the events
/// (plus queued announcements) to the backend. A quiet live frame keeps the stored tree. While
/// it is not live, announcements are only remembered and the tree is dropped, so turning a reader
/// on later reports the whole tree as added.
pub fn sync_screen_reader(world: &mut World) {
    let _span = tracing::trace_span!("ui_a11y_sync").entered();
    // Idle (no reader, nothing queued, nothing to drop): touch nothing, not even the resource.
    let idle = world
        .get_resource::<ScreenReader>()
        .is_none_or(|r| !r.is_live() && !r.was_live && r.pending.is_empty());
    if idle {
        return;
    }
    // Dirty is read before the reader is removed, and it does not watch the reader itself:
    // this function removes and reinserts it every frame it runs.
    let dirty = a11y_inputs_dirty(world);
    let started = sync_clock();
    let Some(mut reader) = world.remove_resource::<ScreenReader>() else {
        return;
    };
    let removed = reader.removals.poll(world);
    #[cfg(test)]
    if removed && let Ok(mut slot) = A11Y_DIRTY_REASON.lock() {
        slot.push_str("removals");
    }
    let dirty = dirty || removed;
    let pending = std::mem::take(&mut reader.pending);
    for (text, politeness) in &pending {
        reader.remember(text, *politeness);
    }
    if !reader.is_live() {
        if reader.was_live {
            reader.tree = A11yTree::default();
            reader.was_live = false;
        }
        world.insert_resource(reader);
        return;
    }
    let became_live = !reader.was_live;
    reader.was_live = true;
    let wants = reader.backend.wants_sync();
    if !became_live && !dirty && pending.is_empty() && !wants {
        world.insert_resource(reader);
        return;
    }
    if !became_live && !dirty {
        let events: Vec<A11yEvent> = pending
            .into_iter()
            .map(|(text, politeness)| A11yEvent::Announcement { text, politeness })
            .collect();
        if !events.is_empty() || wants {
            reader.backend.tree_updated(&reader.tree, &events);
        }
        world.insert_resource(reader);
        return;
    }
    let tree = build_a11y_tree(world);
    let built = sync_clock();
    reader.builds += 1;
    reader.tree_tick = world.change_tick();
    let mut events = tree.diff(&reader.tree);
    let diffed = sync_clock();
    for event in &events {
        if let A11yEvent::Announcement { text, politeness } = event {
            reader.remember(text, *politeness);
        }
    }
    events.extend(
        pending
            .into_iter()
            .map(|(text, politeness)| A11yEvent::Announcement { text, politeness }),
    );
    if !events.is_empty() || reader.backend.wants_sync() {
        reader.backend.tree_updated(&tree, &events);
    }
    reader.tree = tree;
    if let (Some(started), Some(built), Some(diffed), Some(done)) = (started, built, diffed, sync_clock()) {
        reader
            .cost
            .record_stages(done - started, [built - started, diffed - built, done - diffed]);
    }
    world.insert_resource(reader);
}

/// `PreUpdate`, while the reader is live: takes the backend's pending requests and runs each through
/// [`apply_request`], recording how it ended. Costs one backend call per frame while live. The
/// requests are checked against the stored tree when nothing it reads changed since it was built,
/// otherwise against one fresh tree for the frame (ADR-5567).
pub fn route_a11y_requests(world: &mut World) {
    let requests = match world.get_resource_mut::<ScreenReader>() {
        Some(mut reader) if reader.is_live() => reader.backend.drain_requests(),
        _ => return,
    };
    if requests.is_empty() {
        return;
    }
    let _span = tracing::trace_span!("ui_a11y_requests", count = requests.len()).entered();
    let (builds, tree_tick) = world
        .get_resource::<ScreenReader>()
        .map_or((0, Tick::new(0)), |reader| (reader.builds, reader.tree_tick));
    // The stored tree answers only while nothing it reads changed since it was built. A modal
    // opened, a widget disabled or a layer pushed after the last sync (between frames, or by a
    // later system) must refuse this frame's requests as `Inert` / `Disabled`, not reach the
    // control under the dialog with last frame's states (ADR-5567).
    if builds > 0 && !world.last_change_tick_scope(tree_tick, a11y_inputs_dirty) {
        let Some(mut reader) = world.remove_resource::<ScreenReader>() else {
            return;
        };
        for request in requests {
            let result = apply_request_in(world, &reader.tree, &request);
            reader.record_request(request, result);
        }
        world.insert_resource(reader);
        return;
    }
    let tree = build_a11y_tree(world);
    for request in requests {
        let result = apply_request_in(world, &tree, &request);
        if let Some(mut reader) = world.get_resource_mut::<ScreenReader>() {
            reader.record_request(request, result);
        }
    }
}

/// Adds [`ScreenReader`] (unless the host inserted one), [`route_a11y_requests`] and
/// [`sync_screen_reader`]. [`crate::UiPlugin`]
/// adds it.
pub struct ScreenReaderPlugin;

impl Plugin for ScreenReaderPlugin {
    fn build(&self, app: &mut App) {
        // The OS bridge (ADR-1998) unless the host chose a backend.
        #[cfg(feature = "accesskit")]
        crate::accesskit_bridge::install(app);
        app.init_resource::<ScreenReader>()
            .add_systems(
                PreUpdate,
                route_a11y_requests.before(crate::interaction::update_pointer),
            )
            .add_systems(
                PostUpdate,
                sync_screen_reader
                    .after(crate::text::shape_text)
                    .in_set(PostUpdateSet::UiLayout),
            );
    }
}

/// The AT-SPI 2 mapping (Linux): roles, object paths and the D-Bus signals a bridge emits for each
/// [`crate::a11y::A11yEvent`]. [`crate::a11y::atspi::AtSpiStub`] records those signals instead of
/// sending them.
pub mod atspi {
    use std::collections::VecDeque;

    use super::{
        A11yAction, A11yEvent, A11yReject, A11yRequest, A11yRole, A11yStates, A11yTree, PageDirection,
        RequestQueue, ScreenReaderBackend,
    };

    /// How many signals the stub keeps.
    pub const RECORDED: usize = 256;

    /// The AT-SPI role name (the `AtspiRole` enum's names in the AT-SPI 2 reference, lower case with
    /// spaces, as `GetRoleName` returns them).
    pub fn role_name(role: A11yRole) -> &'static str {
        match role {
            A11yRole::Window => "frame",
            A11yRole::Group => "panel",
            A11yRole::Button => "push button",
            A11yRole::CheckBox => "check box",
            A11yRole::Slider => "slider",
            A11yRole::ProgressBar => "progress bar",
            A11yRole::TextInput => "entry",
            A11yRole::ComboBox => "combo box",
            A11yRole::TabList => "page tab list",
            A11yRole::Dialog => "dialog",
            A11yRole::List => "list",
            A11yRole::ScrollView => "scroll pane",
            A11yRole::Label => "label",
            A11yRole::Image => "image",
            A11yRole::Menu => "menu",
            A11yRole::Heading => "heading",
            A11yRole::Region => "landmark",
            A11yRole::Status => "status bar",
            A11yRole::Alert => "alert",
        }
    }

    const PATH_PREFIX: &str = "/org/a11y/atspi/accessible/";

    /// The D-Bus object path of a node.
    pub fn object_path(id: u64) -> String {
        format!("{PATH_PREFIX}{id}")
    }

    /// The node id of an object path made by [`object_path`].
    pub fn parse_object_path(path: &str) -> Option<u64> {
        path.strip_prefix(PATH_PREFIX)?.parse().ok()
    }

    /// The request a bridge makes of an AT-SPI call on `path`: the `Action` interface's names
    /// (`click`, `press`, `activate`, `focus`, `dismiss`, `scroll-into-view`, `page-up`, ...), the
    /// `Value` interface's `increment` / `decrement`, and `set-text` (the EditableText interface)
    /// with its `text`. `None` for a path or action that maps to nothing.
    pub fn request_from_call(path: &str, action: &str, text: Option<&str>) -> Option<A11yRequest> {
        let node = parse_object_path(path)?;
        let action = match action {
            "click" | "press" | "activate" => A11yAction::Press,
            "increment" => A11yAction::Raise,
            "decrement" => A11yAction::Lower,
            "set-text" => A11yAction::SetText(text?.to_owned()),
            "focus" | "grab-focus" => A11yAction::Focus,
            "dismiss" | "close" => A11yAction::Close,
            "scroll-into-view" => A11yAction::ScrollIntoView,
            "page-up" => A11yAction::Page(PageDirection::Up),
            "page-down" => A11yAction::Page(PageDirection::Down),
            "page-left" => A11yAction::Page(PageDirection::Left),
            "page-right" => A11yAction::Page(PageDirection::Right),
            _ => return None,
        };
        Some(A11yRequest { node, action })
    }

    /// One signal the stub would have sent.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct AtSpiSignal {
        /// The event name, e.g. `object:state-changed:focused`.
        pub event: String,
        /// The object path the signal is sent from.
        pub path: String,
        /// `detail1` (state on = 1, off = 0; child index; politeness for announcements).
        pub detail1: i32,
        /// The `any_data` payload as text (child path, new name, announcement text).
        pub data: String,
    }

    impl std::fmt::Display for AtSpiSignal {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{} {} {} {}", self.event, self.path, self.detail1, self.data)
        }
    }

    /// A recording AT-SPI backend: maps events to signals and keeps the last [`RECORDED`].
    #[derive(Debug, Default)]
    pub struct AtSpiStub {
        active: bool,
        signals: VecDeque<AtSpiSignal>,
        requests: RequestQueue,
        /// Signals produced in total (the ring keeps the newest).
        pub total: u64,
    }

    impl AtSpiStub {
        /// A stub that reports itself active (or not).
        pub fn new(active: bool) -> Self {
            Self {
                active,
                ..Self::default()
            }
        }

        /// Active when `REGIA_SCREEN_READER=1`. A real bridge asks the session bus for the
        /// accessibility bus (`org.a11y.Bus`) and its `IsEnabled` property instead.
        pub fn detect() -> Self {
            Self::new(regia_core::brand::env_var("REGIA_SCREEN_READER").is_ok_and(|v| v == "1"))
        }

        /// Turns the stub on or off.
        pub fn set_active(&mut self, active: bool) {
            self.active = active;
        }

        /// The queue a bridge pushes [`request_from_call`] results into; the stub hands them to the
        /// UI each frame.
        pub fn request_queue(&self) -> RequestQueue {
            self.requests.clone()
        }

        /// The recorded signals, oldest first.
        pub fn signals(&self) -> impl Iterator<Item = &AtSpiSignal> {
            self.signals.iter()
        }

        fn push(&mut self, event: &str, id: u64, detail1: i32, data: String) {
            if self.signals.len() == RECORDED {
                self.signals.pop_front();
            }
            self.total += 1;
            self.signals.push_back(AtSpiSignal {
                event: event.to_owned(),
                path: object_path(id),
                detail1,
                data,
            });
        }

        fn states(&mut self, id: u64, before: A11yStates, now: A11yStates) {
            let pairs = [
                ("focused", before.focused, now.focused),
                ("focusable", before.focusable, now.focusable),
                // AT-SPI speaks of enabled / sensitive, the opposite of disabled.
                ("enabled", !before.disabled, !now.disabled),
                ("sensitive", !before.disabled, !now.disabled),
                ("selected", before.selected, now.selected),
                ("invalid-entry", before.invalid, now.invalid),
                ("checked", before.checked == Some(true), now.checked == Some(true)),
                (
                    "expanded",
                    before.expanded == Some(true),
                    now.expanded == Some(true),
                ),
                ("read-only", before.read_only, now.read_only),
            ];
            for (state, was, is) in pairs {
                if was != is {
                    self.push(
                        &format!("object:state-changed:{state}"),
                        id,
                        i32::from(is),
                        String::new(),
                    );
                }
            }
        }
    }

    impl ScreenReaderBackend for AtSpiStub {
        fn name(&self) -> &'static str {
            "atspi_stub"
        }

        fn is_active(&self) -> bool {
            self.active
        }

        fn tree_updated(&mut self, tree: &A11yTree, events: &[A11yEvent]) {
            for event in events {
                match event {
                    A11yEvent::Added { id, parent } => {
                        let index = parent
                            .and_then(|p| tree.get(p))
                            .and_then(|p| p.children.iter().position(|c| c == id))
                            .map_or(-1, |i| i32::try_from(i).unwrap_or(i32::MAX));
                        let from = parent.unwrap_or(0);
                        self.push("object:children-changed:add", from, index, object_path(*id));
                    }
                    A11yEvent::Removed { id, parent } => {
                        self.push(
                            "object:children-changed:remove",
                            parent.unwrap_or(0),
                            -1,
                            object_path(*id),
                        );
                    }
                    A11yEvent::NameChanged { id } => {
                        let name = tree.get(*id).map(|n| n.name.clone()).unwrap_or_default();
                        self.push("object:property-change:accessible-name", *id, 0, name);
                    }
                    A11yEvent::ValueChanged { id } => {
                        let value = tree.get(*id).and_then(|n| n.value.clone()).unwrap_or_default();
                        self.push("object:property-change:accessible-value", *id, 0, value);
                    }
                    A11yEvent::StatesChanged { id, before } => {
                        if let Some(node) = tree.get(*id) {
                            // Focus is reported once, by FocusChanged.
                            let mut now = node.states;
                            now.focused = before.focused;
                            self.states(*id, *before, now);
                        }
                    }
                    A11yEvent::FocusChanged { from, to } => {
                        if let Some(from) = from {
                            self.push("object:state-changed:focused", *from, 0, String::new());
                        }
                        if let Some(to) = to {
                            let summary = tree
                                .get(*to)
                                .map(super::A11yNode::spoken_summary)
                                .unwrap_or_default();
                            self.push("object:state-changed:focused", *to, 1, summary);
                        }
                    }
                    A11yEvent::ScreenChanged { to, .. } => {
                        let target = to.or_else(|| tree.nodes.first().map(|n| n.id)).unwrap_or(0);
                        self.push("window:activate", target, 0, String::new());
                    }
                    A11yEvent::Announcement { text, politeness } => {
                        let level = match politeness {
                            super::Politeness::Assertive => 2,
                            _ => 1,
                        };
                        let root = tree.nodes.first().map_or(0, |n| n.id);
                        self.push("object:announcement", root, level, text.clone());
                    }
                }
            }
        }

        fn recent_events(&self) -> Vec<String> {
            self.signals.iter().map(ToString::to_string).collect()
        }

        fn drain_requests(&mut self) -> Vec<A11yRequest> {
            self.requests.take()
        }

        fn request_result(&mut self, request: &A11yRequest, result: Result<(), A11yReject>) {
            // A real bridge answers the D-Bus call; the stub records the outcome as a signal.
            let outcome = result.map_or_else(|reason| reason.reason(), |()| "ok");
            self.push(
                "request:result",
                request.node,
                i32::from(result.is_ok()),
                format!("{} {outcome}", request.action.name()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The release-build cost of a connected screen reader is reported from where it runs
    /// (ADR-5379): exact mean, nearest-rank p99 and max over the last [`SYNC_COST_WINDOW`] syncs.
    #[test]
    fn sync_cost_reports_mean_p99_and_max_over_its_window() {
        use std::time::Duration;
        let mut cost = SyncCost::default();
        assert_eq!(
            (
                cost.samples(),
                cost.mean_ms(),
                cost.percentile_ms(99.0),
                cost.max_ms()
            ),
            (0, 0.0, 0.0, 0.0)
        );
        // 99 syncs of 0.2 ms and one of 5 ms: p99 is still 0.2 ms, the max is 5 ms.
        for _ in 0..99 {
            cost.record(Duration::from_micros(200));
        }
        cost.record(Duration::from_millis(5));
        assert_eq!(cost.samples(), 100);
        assert!((cost.mean_ms() - 0.248).abs() < 1e-9, "{}", cost.mean_ms());
        assert_eq!(cost.percentile_ms(99.0), 0.2);
        assert_eq!(cost.percentile_ms(100.0), 5.0);
        assert_eq!(cost.max_ms(), 5.0);
        // The window keeps the newest samples only.
        for _ in 0..SYNC_COST_WINDOW {
            cost.record(Duration::from_micros(100));
        }
        assert_eq!(cost.samples(), SYNC_COST_WINDOW);
        assert_eq!(cost.total(), 100 + SYNC_COST_WINDOW as u64);
        assert_eq!(cost.max_ms(), 0.1);
        assert_eq!(cost.stage_mean_ms(SyncStage::Build), 0.0, "record() has no split");
        let mut split = SyncCost::default();
        split.record_stages(
            Duration::from_micros(900),
            [
                Duration::from_micros(600),
                Duration::from_micros(100),
                Duration::from_micros(200),
            ],
        );
        let stages = SyncStage::ALL.map(|s| split.stage_mean_ms(s));
        assert_eq!(stages, [0.6, 0.1, 0.2]);
        assert_eq!(split.mean_ms(), 0.9);
    }

    #[test]
    fn only_live_syncs_are_timed() {
        let mut world = World::new();
        world.insert_resource(ScreenReader::with_backend(Box::new(NullScreenReader)));
        sync_screen_reader(&mut world);
        assert_eq!(
            world.resource::<ScreenReader>().sync_cost().total(),
            0,
            "idle frames are not timed"
        );
        world.resource_mut::<ScreenReader>().set_exporting(true);
        sync_screen_reader(&mut world);
        // A live sync with no changed input reuses the tree (it does not rebuild), so the
        // second sync gets a changed input: a focus state arriving.
        world.insert_resource(crate::focus::FocusState::default());
        sync_screen_reader(&mut world);
        let reader = world.resource::<ScreenReader>();
        assert_eq!((reader.sync_cost().total(), reader.builds), (2, 2));
    }

    fn node(id: u64, name: &str) -> A11yNode {
        A11yNode {
            id,
            role: Some(A11yRole::Button),
            name: name.to_owned(),
            ..A11yNode::default()
        }
    }

    #[test]
    fn focusable_text_keeps_its_node_and_a_focusable_node_takes_its_label() {
        let mut world = World::new();
        let root = world.spawn(UiRoot::default()).id();
        let list = world
            .spawn((crate::node::UiNode, ListView::default(), ChildOf(root)))
            .id();
        // A list row with its own text takes focus: folding it would hide it from the reader.
        let row = world
            .spawn((
                crate::node::UiNode,
                Text::new("Row one"),
                Focusable::default(),
                ChildOf(list),
            ))
            .id();
        // A recycled row keeps an empty text and shows its label in a child.
        let drawn = world
            .spawn((
                crate::node::UiNode,
                Text::new(""),
                Focusable::default(),
                ChildOf(list),
            ))
            .id();
        world.spawn((crate::node::UiNode, Text::new("Row two"), ChildOf(drawn)));
        // A tab: no widget role, but it takes focus and shows a label.
        let tab = world
            .spawn((crate::node::UiNode, Focusable::default(), ChildOf(root)))
            .id();
        world.spawn((crate::node::UiNode, Text::new("Audio"), ChildOf(tab)));
        let tree = build_a11y_tree(&mut world);
        let name = |e: Entity| tree.get(e.to_bits()).map(|n| n.name.clone());
        assert_eq!(name(row).as_deref(), Some("Row one"));
        assert_eq!(name(drawn).as_deref(), Some("Row two"));
        assert_eq!(name(tab).as_deref(), Some("Audio"));
        assert_eq!(name(list).as_deref(), Some(""), "rows do not name their list");
    }

    #[test]
    fn role_and_politeness_names_round_trip() {
        for role in A11yRole::ALL {
            assert_eq!(A11yRole::parse(role.name()), Some(role));
            assert!(
                role.name().bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{}",
                role.name()
            );
        }
        for p in [Politeness::Off, Politeness::Polite, Politeness::Assertive] {
            assert_eq!(Politeness::parse(p.name()), Some(p));
        }
        assert_eq!(A11yRole::parse("widget"), None);
    }

    #[test]
    fn spoken_summaries_read_name_role_value_and_states() {
        let mut slider = node(1, "Volume");
        slider.role = Some(A11yRole::Slider);
        slider.value = Some("50".to_owned());
        assert_eq!(slider.spoken_summary(), "Volume, slider, 50");
        let mut toggle = node(2, "Subtitles");
        toggle.role = Some(A11yRole::CheckBox);
        toggle.states.checked = Some(true);
        assert_eq!(toggle.spoken_summary(), "Subtitles, check box, checked");
        toggle.states.checked = Some(false);
        assert_eq!(toggle.spoken_summary(), "Subtitles, check box, not checked");
        let mut start = node(3, "Start");
        start.states.disabled = true;
        assert_eq!(start.spoken_summary(), "Start, button, disabled");
        let mut combo = node(4, "Quality");
        combo.role = Some(A11yRole::ComboBox);
        combo.value = Some("High".to_owned());
        combo.states.expanded = Some(false);
        assert_eq!(combo.spoken_summary(), "Quality, combo box, High, collapsed");
        let unnamed = A11yNode::default();
        assert_eq!(unnamed.spoken_summary(), "group");
    }

    #[test]
    fn diff_reports_each_kind_of_change_once_in_order() {
        let mut before = A11yTree::default();
        before.nodes = vec![node(1, "Start"), node(2, "Quit")];
        before.focus = Some(1);
        let mut after = before.clone();
        after.nodes.remove(1); // Quit gone
        after.nodes[0].name = "Continue".to_owned();
        after.nodes[0].states.disabled = true;
        let mut status = node(3, "Saved");
        status.role = Some(A11yRole::Status);
        status.live = Politeness::Polite;
        after.nodes.push(status);
        after.focus = None;
        let events = after.diff(&before);
        let kinds: Vec<&str> = events.iter().map(A11yEvent::kind).collect();
        assert_eq!(
            kinds,
            vec![
                "removed",
                "name_changed",
                "states_changed",
                "added",
                "focus_changed"
            ]
        );
        // A live region announces its change, not its first appearance.
        let mut later = after.clone();
        later.nodes[1].name = "Saved 2 files".to_owned();
        let events = later.diff(&after);
        assert_eq!(
            events,
            vec![
                A11yEvent::NameChanged { id: 3 },
                A11yEvent::Announcement {
                    text: "Saved 2 files".to_owned(),
                    politeness: Politeness::Polite
                }
            ]
        );
        assert!(later.diff(&later).is_empty(), "no change, no events");
    }

    #[test]
    fn atspi_stub_maps_events_to_signals() {
        let mut stub = atspi::AtSpiStub::new(true);
        let mut tree = A11yTree::default();
        let mut root = node(1, "");
        root.role = Some(A11yRole::Window);
        root.children = vec![2];
        let mut toggle = node(2, "Subtitles");
        toggle.role = Some(A11yRole::CheckBox);
        toggle.parent = Some(1);
        toggle.states.checked = Some(true);
        tree.nodes = vec![root, toggle];
        tree.focus = Some(2);
        let events = tree.diff(&A11yTree::default());
        stub.tree_updated(&tree, &events);
        let mut unchecked = tree.clone();
        unchecked.nodes[1].states.checked = Some(false);
        stub.tree_updated(&unchecked, &unchecked.diff(&tree));
        stub.tree_updated(
            &unchecked,
            &[A11yEvent::Announcement {
                text: "Checkpoint".to_owned(),
                politeness: Politeness::Assertive,
            }],
        );
        let signals: Vec<String> = stub.recent_events();
        assert_eq!(
            signals,
            vec![
                "object:children-changed:add /org/a11y/atspi/accessible/0 -1 /org/a11y/atspi/accessible/1",
                "object:children-changed:add /org/a11y/atspi/accessible/1 0 /org/a11y/atspi/accessible/2",
                "object:state-changed:focused /org/a11y/atspi/accessible/2 1 Subtitles, check box, checked",
                "object:state-changed:checked /org/a11y/atspi/accessible/2 0 ",
                "object:announcement /org/a11y/atspi/accessible/1 2 Checkpoint",
            ]
        );
        assert_eq!(atspi::role_name(A11yRole::Button), "push button");
        assert_eq!(atspi::role_name(A11yRole::TextInput), "entry");
        assert_eq!(stub.total, 5);
    }
    #[test]
    fn hints_are_read_last_and_the_top_layer_change_is_one_event() {
        let mut button = node(1, "Back");
        button.description = "Previous screen".to_owned();
        button.hint = "Double tap to go back".to_owned();
        assert_eq!(
            button.spoken_summary(),
            "Back, button, Previous screen, Double tap to go back"
        );
        let mut before = A11yTree::default();
        before.nodes = vec![node(1, "Back")];
        before.focus = Some(1);
        let mut after = before.clone();
        after.top_layer = Some(7);
        after.focus = None;
        assert_eq!(
            after.diff(&before),
            vec![
                A11yEvent::ScreenChanged {
                    from: None,
                    to: Some(7)
                },
                A11yEvent::FocusChanged {
                    from: Some(1),
                    to: None
                }
            ]
        );
        assert_eq!(
            A11yEvent::ScreenChanged { from: None, to: None }.kind(),
            "screen_changed"
        );
    }

    #[test]
    fn requests_queue_in_order_and_drain_once() {
        let (mut reader, handle) = RecordingReader::new();
        handle.request(1, A11yAction::Press);
        handle.request(2, A11yAction::Page(PageDirection::Down));
        let drained = reader.drain_requests();
        assert_eq!(
            drained
                .iter()
                .map(|r| (r.node, r.action.name()))
                .collect::<Vec<_>>(),
            vec![(1, "press"), (2, "page_down")]
        );
        assert!(reader.drain_requests().is_empty());
        reader.request_result(&drained[0], Err(A11yReject::Disabled));
        assert_eq!(handle.results().len(), 1);
    }

    /// A live reader that already exported the tree keeps that tree on a quiet frame, and a
    /// request uses it instead of walking the widgets again.
    #[test]
    fn live_reader_keeps_the_tree_when_nothing_changed() {
        let (reader, handle) = RecordingReader::new();
        let mut world = World::new();
        world.insert_resource(ScreenReader::with_backend(Box::new(reader)));
        let root = world.spawn(UiRoot::default()).id();
        let button = world
            .spawn((crate::node::UiNode, Button, Text::new("Play"), ChildOf(root)))
            .id();
        sync_screen_reader(&mut world);
        assert_eq!(world.resource::<ScreenReader>().builds, 1);
        let name_ptr = world
            .resource::<ScreenReader>()
            .tree()
            .get(button.to_bits())
            .expect("button")
            .name
            .as_ptr();
        world.clear_trackers();
        let walked = a11y_tree_builds();
        sync_screen_reader(&mut world);
        assert_eq!(
            world.resource::<ScreenReader>().builds,
            1,
            "a quiet frame rebuilt the tree"
        );
        assert_eq!(a11y_tree_builds(), walked, "a quiet frame walked the UI");
        assert_eq!(
            world
                .resource::<ScreenReader>()
                .tree()
                .get(button.to_bits())
                .expect("button")
                .name
                .as_ptr(),
            name_ptr,
            "a quiet frame replaced the stored tree"
        );

        handle.request(0, A11yAction::Press);
        let before_route = a11y_tree_builds();
        route_a11y_requests(&mut world);
        assert_eq!(a11y_tree_builds(), before_route, "a request rebuilt the tree");
        assert_eq!(world.resource::<ScreenReader>().builds, 1);

        world.clear_trackers();
        world.get_mut::<Text>(button).expect("text").spans[0].content = "Pause".into();
        handle.clear_events();
        sync_screen_reader(&mut world);
        assert_eq!(world.resource::<ScreenReader>().builds, 2);
        let events = handle.events();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, A11yEvent::NameChanged { id } if *id == button.to_bits())),
            "{events:?}"
        );
    }

    /// A widget disabled after the last sync (here between frames) refuses this frame's request:
    /// the router must not answer from the stored tree's old states (ADR-5567).
    #[test]
    fn a_change_after_the_sync_is_seen_by_the_next_request() {
        let (reader, handle) = RecordingReader::new();
        let mut world = World::new();
        world.insert_resource(ScreenReader::with_backend(Box::new(reader)));
        let root = world.spawn(UiRoot::default()).id();
        let button = world
            .spawn((crate::node::UiNode, Button, Text::new("Play"), ChildOf(root)))
            .id();
        sync_screen_reader(&mut world);
        assert_eq!(world.resource::<ScreenReader>().builds, 1);
        world.clear_trackers();
        world.entity_mut(button).insert(Disabled);
        handle.request(button.to_bits(), A11yAction::Press);
        route_a11y_requests(&mut world);
        let results: Vec<_> = handle.results().into_iter().map(|(_, r)| r).collect();
        assert_eq!(results, vec![Err(A11yReject::Disabled)]);
        // A quiet frame after that still serves requests from the stored tree.
        sync_screen_reader(&mut world);
        world.clear_trackers();
        let builds = world.resource::<ScreenReader>().builds;
        let walked = a11y_tree_builds();
        handle.request(button.to_bits(), A11yAction::Press);
        route_a11y_requests(&mut world);
        assert_eq!(a11y_tree_builds(), walked, "a quiet request rebuilt the tree");
        assert_eq!(world.resource::<ScreenReader>().builds, builds);
        let last = handle.results().last().map(|(_, r)| *r);
        assert_eq!(last, Some(Err(A11yReject::Disabled)));
    }

    /// The settings menu from `tests/a11y_tree.rs`, driven by the whole UI plugin, must not rebuild
    /// on a second frame that changes nothing. The failure names the component that stayed dirty.
    #[test]
    fn live_plugin_menu_stays_quiet_on_the_second_frame() {
        let mut app = App::new();
        app.insert_resource(crate::text::TextSystem::embedded_only());
        app.insert_resource(ScreenReader::with_backend(Box::new(atspi::AtSpiStub::new(true))));
        app.add_plugins(crate::UiPlugin::code_only());
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
        let world = app.world_mut();
        let root = world
            .spawn((crate::node::UiRoot::default(), crate::node::UiNode))
            .id();
        let column = world.spawn((crate::node::UiNode, ChildOf(root))).id();
        let start = world
            .spawn((
                crate::node::UiNode,
                crate::widgets::Button,
                crate::focus::Focusable::default(),
                ChildOf(column),
            ))
            .id();
        world.spawn((
            crate::node::UiNode,
            crate::text::Text::new("Start"),
            ChildOf(start),
        ));
        let toggle = world
            .spawn((
                crate::node::UiNode,
                crate::widgets::Toggle { checked: true },
                crate::focus::Focusable::default(),
                ChildOf(column),
            ))
            .id();
        world.spawn((
            crate::node::UiNode,
            crate::text::Text::new("Subtitles"),
            ChildOf(toggle),
        ));
        world.spawn((
            crate::node::UiNode,
            crate::widgets::Slider {
                value: 50.0,
                min: 0.0,
                max: 100.0,
                ..crate::widgets::Slider::default()
            },
            Accessible::default().with_label("Volume"),
            ChildOf(column),
        ));
        let mut name_field = crate::text_input::TextInput::default();
        name_field.text = "Ada".to_owned();
        name_field.placeholder = "Player name".to_owned();
        world.spawn((crate::node::UiNode, name_field, ChildOf(column)));
        world.spawn((
            crate::node::UiNode,
            crate::text::Text::new("Version 1.0"),
            ChildOf(column),
        ));
        world.spawn((
            crate::node::UiNode,
            crate::text::Text::new("Ready"),
            Accessible::default()
                .with_role(A11yRole::Status)
                .with_live(Politeness::Polite),
            ChildOf(column),
        ));
        let hidden = world
            .spawn((
                crate::node::UiNode,
                crate::node::NodeStyle(crate::style::Style {
                    display: crate::style::Display::None,
                    ..crate::style::Style::default()
                }),
                ChildOf(column),
            ))
            .id();
        world.spawn((crate::node::UiNode, crate::widgets::Button, ChildOf(hidden)));
        world.spawn((
            crate::node::UiNode,
            crate::text::Text::new("*"),
            Accessible::decorative(),
            ChildOf(column),
        ));

        app.update();
        assert_eq!(app.world().resource::<ScreenReader>().builds, 1);
        app.update();
        let reason = A11Y_DIRTY_REASON.lock().expect("reason").clone();
        assert_eq!(
            app.world().resource::<ScreenReader>().builds,
            1,
            "quiet frame rebuilt because [{reason}]"
        );
    }

    /// A removed component or a despawned node changes the tree, but `Changed` never sees it: a
    /// re-enabled button stayed "disabled" and a closed menu stayed in the reader's tree. A
    /// removal after last frame's sync (here: before the tracker clear) must count too.
    #[test]
    fn live_reader_rebuilds_after_a_removal_or_a_despawn() {
        let (reader, _handle) = RecordingReader::new();
        let mut world = World::new();
        world.insert_resource(ScreenReader::with_backend(Box::new(reader)));
        let root = world.spawn(UiRoot::default()).id();
        let button = world
            .spawn((
                crate::node::UiNode,
                Button,
                Disabled,
                Text::new("Play"),
                ChildOf(root),
            ))
            .id();
        let extra = world
            .spawn((crate::node::UiNode, Button, Text::new("Quit"), ChildOf(root)))
            .id();
        sync_screen_reader(&mut world);
        let node =
            |world: &World, e: Entity| world.resource::<ScreenReader>().tree().get(e.to_bits()).cloned();
        assert!(node(&world, button).expect("button").states.disabled);
        world.clear_trackers();

        // Same frame: the button is enabled before the sync.
        world.entity_mut(button).remove::<Disabled>();
        sync_screen_reader(&mut world);
        assert!(
            !node(&world, button).expect("button").states.disabled,
            "the reader still says disabled"
        );

        // Late in a frame (after its sync): a despawn the next frame's sync must still see.
        world.clear_trackers();
        world.despawn(extra);
        world.clear_trackers();
        sync_screen_reader(&mut world);
        assert!(
            node(&world, extra).is_none(),
            "the reader still lists a despawned node"
        );

        // The quiet frame after that stays quiet.
        world.clear_trackers();
        let builds = world.resource::<ScreenReader>().builds;
        sync_screen_reader(&mut world);
        assert_eq!(world.resource::<ScreenReader>().builds, builds);
    }
}
