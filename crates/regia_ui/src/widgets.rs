// SPDX-License-Identifier: MPL-2.0
//! Widgets (`specs/18` § 1): Button, Toggle, Slider, ProgressBar, TextInput, Dropdown, TabBar,
//! Modal, Tooltip, VirtualStick and VirtualButton — components + the systems that react to
//! interaction and focus events. Dropdown lists, tooltips and menus ([`crate::menu`]) open on the
//! popup layer ([`crate::popup`]).

use regia_app::prelude::*;

use crate::interaction::{UiEvent, UiEventKind, UiPointer, UiValue};
use crate::layout::LayoutDirty;
use crate::node::{Classes, ComputedNode, Interactable, NodeStyle, UiNode};
use crate::style::{Align, Dimension, Display, Edges, FlexDirection, JustifyContent, Style};
use crate::text::{Text, TextSpan};
use crate::theme::Themed;

pub use crate::text_input::TextInput;

/// A push button: `Interactable` + children (typically one `Text`). Clicks (pointer or `Submit`
/// while focused) re-emit [`UiEventKind::Click`].
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Button;

/// A two-state switch. Click (or `Submit`) toggles `checked` and emits `ValueChanged(Bool)`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Toggle {
    /// Current state.
    pub checked: bool,
}

/// A draggable value slider. Emits `ValueChanged(Number)`.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Slider {
    /// Current value.
    pub value: f32,
    /// Minimum.
    pub min: f32,
    /// Maximum.
    pub max: f32,
    /// Step (0 = continuous).
    pub step: f32,
}

impl Default for Slider {
    fn default() -> Self {
        Self {
            value: 0.5,
            min: 0.0,
            max: 1.0,
            step: 0.0,
        }
    }
}

impl Slider {
    /// Clamped, stepped value from a `0..1` fraction.
    ///
    /// Reversed ends are sorted; non-finite ends yield 0 (authored data may hold either).
    pub fn value_at_fraction(&self, fraction: f32) -> f32 {
        // Check the ends before sorting: `f32::min` drops a single NaN end silently.
        if !self.min.is_finite() || !self.max.is_finite() {
            return 0.0;
        }
        let (lo, hi) = (self.min.min(self.max), self.min.max(self.max));
        let fraction = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mut v = lo + (hi - lo) * fraction;
        if self.step > 0.0 && self.step.is_finite() {
            v = (v / self.step).round() * self.step;
        }
        // A range wider than `f32::MAX` overflows to infinity: fall back to the low end.
        if v.is_finite() { v.clamp(lo, hi) } else { lo }
    }

    /// The clamped, stepped value nearest `value`.
    ///
    /// The fraction runs from the lower end, as [`Slider::value_at_fraction`] sorts the ends:
    /// measured from `min`, every change on a reversed slider (`min` 10, `max` 0) landed on its
    /// low end (audit-seq).
    pub(crate) fn value_near(&self, value: f32) -> f32 {
        let span = (self.max - self.min).abs().max(1e-6);
        self.value_at_fraction((value - self.min.min(self.max)) / span)
    }

    /// The `0..1` fraction of the current value.
    pub fn fraction(&self) -> f32 {
        if (self.max - self.min).abs() < 1e-6 {
            return 0.0;
        }
        ((self.value - self.min) / (self.max - self.min)).clamp(0.0, 1.0)
    }
}

/// Progress indicator: extract draws an accent fill over the node, `value` of its width
/// (a value change redraws; it never relayouts).
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct ProgressBar {
    /// Progress `0..=1` (clamped).
    pub value: f32,
}

impl Default for ProgressBar {
    fn default() -> Self {
        Self { value: 0.0 }
    }
}

/// A dropdown (select) control. Emits `ValueChanged(Index)` on selection.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct Dropdown {
    /// Options.
    pub options: Vec<String>,
    /// Selected index.
    pub selected: Option<usize>,
    /// Popup open.
    pub open: bool,
}

/// A tab bar. Emits `ValueChanged(Index)` when the active tab changes.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct TabBar {
    /// Tab labels.
    pub tabs: Vec<String>,
    /// Active tab index.
    pub active: usize,
}

/// A modal dialog: dims the screen, traps focus (its subtree is a focus scope) and closes on
/// `Cancel`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modal {
    /// Whether the modal is open.
    pub open: bool,
    /// Close when `Cancel` is pressed / the scrim is clicked.
    pub close_on_cancel: bool,
}

impl Default for Modal {
    fn default() -> Self {
        Self {
            open: true,
            close_on_cancel: true,
        }
    }
}

/// A tooltip on the node that carries it, shown on the popup layer ([`crate::popup`]) after the
/// pointer hovers the node for `delay` seconds, or after the node holds focus in navigation mode
/// (keyboard, gamepad) for `focus_delay` seconds. The rules (warm-up, dismiss on press, cancel and
/// scroll) live in [`crate::hints`].
#[derive(Component, Debug, Clone, PartialEq)]
pub struct Tooltip {
    /// Tooltip text.
    pub text: String,
    /// Hover delay in seconds.
    pub delay: f32,
    /// Seconds waited so far (managed by the system).
    pub(crate) elapsed: f32,
    /// Whether currently visible.
    pub visible: bool,
    /// Focus delay in seconds (navigation mode).
    pub focus_delay: f32,
    /// The open tooltip popup.
    pub(crate) popup: Option<Entity>,
    /// Kept open by an agent (`ui.open_popup`) until `ui.close_popup`.
    pub(crate) pinned: bool,
}

impl Tooltip {
    /// A tooltip with the default 0.5 s delay.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }
}

impl Default for Tooltip {
    fn default() -> Self {
        Self {
            text: String::new(),
            delay: 0.5,
            elapsed: 0.0,
            visible: false,
            focus_delay: 0.8,
            popup: None,
            pinned: false,
        }
    }
}

/// Virtual thumbstick for touch input (`specs/18` § 1): writes its deflection into the
/// [`VirtualAxes`] resource under `action`, for the input layer to merge into gameplay actions.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct VirtualStick {
    /// Action name (e.g. `"Move"`).
    pub action: String,
    /// Stick radius in UI px.
    pub radius: f32,
    /// Current deflection `-1..=1` per axis (x right, y up).
    pub value: [f32; 2],
    /// The touch driving the stick, if any.
    pub(crate) touch: Option<u64>,
}

impl Default for VirtualStick {
    fn default() -> Self {
        Self {
            action: String::new(),
            radius: 64.0,
            value: [0.0; 2],
            touch: None,
        }
    }
}

/// Virtual touch button (`specs/18` § 1): sets `action` in [`VirtualButtons`] while pressed.
#[derive(Component, Debug, Clone, PartialEq, Eq, Default)]
pub struct VirtualButton {
    /// Action name (e.g. `"Jump"`).
    pub action: String,
}

/// Live virtual axis values by action (written by `VirtualStick`, read by gameplay/input).
#[derive(Resource, Debug, Default)]
pub struct VirtualAxes(pub std::collections::HashMap<String, [f32; 2]>);

/// Live virtual button states by action.
#[derive(Resource, Debug, Default)]
pub struct VirtualButtons(pub std::collections::HashMap<String, bool>);

/// `Update`: all widget state machines, reacting to interaction events and focused keyboard
/// input.
pub fn ui_widgets(world: &mut World) {
    let _span = tracing::trace_span!("ui_widgets").entered();
    // Popups first: closing on outside presses, cancel and despawned anchors.
    crate::popup::popup_input(world);
    let input = crate::menu::read_frame_input(world);
    sync_tab_bar_children(world);
    sync_dropdown_labels(world);
    widget_buttons_toggles(world);
    widget_sliders(world);
    crate::text_input::ui_text_inputs(world);
    widget_dropdowns(world, &input);
    crate::menu::ui_menus(world, &input);
    widget_tab_bars(world);
    widget_modals(world);
    widget_tooltips(world);
    widget_virtual_controls(world);
    crate::default_widgets::default_widgets(world);
    crate::rebind_panel::rebind_panels(world);
    // Last: place what opened or moved, so this frame's layout already has it in place.
    crate::popup::place_popups(world);
}

/// Tab `index`'s label of the [`TabBar`] on `entity`, cloned (the caller writes it).
fn tab_label(world: &World, entity: Entity, index: usize) -> Option<String> {
    world
        .get::<TabBar>(entity)
        .and_then(|bar| bar.tabs.get(index).cloned())
}

/// Ensures each [`TabBar`] has one focusable tab button child per label.
fn sync_tab_bar_children(world: &mut World) {
    let bars: Vec<Entity> = world
        .query_filtered::<Entity, With<TabBar>>()
        .iter(world)
        .collect();
    for entity in bars {
        let Some((tab_count, active)) = world
            .get::<TabBar>(entity)
            .map(|bar| (bar.tabs.len(), bar.active))
        else {
            continue;
        };
        let child_tabs: Vec<Entity> = world
            .get::<Children>(entity)
            .map(|c| {
                c.iter()
                    .filter(|ch| world.get::<TabIndex>(*ch).is_some())
                    .collect()
            })
            .unwrap_or_default();
        if child_tabs.len() == tab_count {
            // Refresh labels / active class. Label text lives on a child (centred by flexbox).
            for (i, child) in child_tabs.into_iter().enumerate() {
                let label_entity = world
                    .get::<Children>(child)
                    .and_then(|c| c.iter().find(|e| world.get::<Text>(*e).is_some()));
                let target = label_entity.or_else(|| world.get::<Text>(child).map(|_| child));
                // Compared in place: an idle tab clones no label.
                let stale = target.is_some_and(|text_entity| {
                    let current = world.get::<Text>(text_entity).and_then(first_lit);
                    world
                        .get::<TabBar>(entity)
                        .is_none_or(|bar| current != bar.tabs.get(i).map(String::as_str))
                });
                if stale && let Some(label) = tab_label(world, entity, i) {
                    count_widget_text_clone(world);
                    let text_entity = target.unwrap_or(child);
                    if let Some(mut text) = world.get_mut::<Text>(text_entity) {
                        text.spans = vec![TextSpan::lit(label)];
                    }
                }
                let want_primary = i == active;
                let has_primary = world
                    .get::<Classes>(child)
                    .is_some_and(|classes| classes.0.iter().any(|class| class == "primary"));
                if want_primary != has_primary {
                    let mut classes = world.get::<Classes>(child).cloned().unwrap_or_default();
                    classes.0.retain(|class| class != "primary");
                    if want_primary {
                        classes.0.push("primary".into());
                    }
                    world.entity_mut(child).insert(classes);
                    world.resource_mut::<LayoutDirty>().mark();
                }
            }
            continue;
        }
        for child in child_tabs {
            world.entity_mut(child).despawn();
        }
        for i in 0..tab_count {
            let Some(label) = tab_label(world, entity, i) else {
                continue;
            };
            let mut classes = vec!["button".to_owned(), "tab".to_owned()];
            if i == active {
                classes.push("primary".into());
            }
            let child = world
                .spawn((
                    UiNode,
                    NodeStyle::new(Style {
                        padding: Edges::px(8.0),
                        justify_content: JustifyContent::Center,
                        align_items: Align::Center,
                        flex_grow: 1.0,
                        min_size: [Dimension::Auto, Dimension::Points(32.0)],
                        ..Default::default()
                    }),
                    Interactable::default(),
                    crate::focus::Focusable::default(),
                    TabIndex(i),
                    Classes(classes),
                    Themed::default(),
                ))
                .with_children(|tab| {
                    tab.spawn((
                        UiNode,
                        Text {
                            spans: vec![TextSpan::lit(label.clone())],
                            wrap: crate::text::WrapMode::None,
                            ..Default::default()
                        },
                    ));
                })
                .id();
            world.entity_mut(entity).add_child(child);
        }
        world.resource_mut::<LayoutDirty>().mark();
    }
}

/// The selected option, or the placeholder, without cloning the option list.
fn dropdown_label(dropdown: &Dropdown) -> &str {
    dropdown
        .selected
        .and_then(|index| dropdown.options.get(index).map(String::as_str))
        .unwrap_or("Select…")
}

/// Keeps a dropdown's first text child showing the selected option (or a placeholder).
fn sync_dropdown_labels(world: &mut World) {
    let dropdowns: Vec<Entity> = world
        .query_filtered::<Entity, With<Dropdown>>()
        .iter(world)
        .collect();
    for entity in dropdowns {
        let text_child = world
            .get::<Children>(entity)
            .and_then(|c| c.iter().find(|child| world.get::<Text>(*child).is_some()));
        // Compared in place: an idle dropdown clones no label.
        let stale = text_child.is_none_or(|child| {
            let current = world.get::<Text>(child).and_then(first_lit);
            world
                .get::<Dropdown>(entity)
                .is_none_or(|dropdown| current != Some(dropdown_label(dropdown)))
        });
        if !stale {
            continue;
        }
        let Some(label) = world
            .get::<Dropdown>(entity)
            .map(|dropdown| dropdown_label(dropdown).to_owned())
        else {
            continue;
        };
        count_widget_text_clone(world);
        match text_child {
            Some(child) => {
                if let Some(mut text) = world.get_mut::<Text>(child) {
                    text.spans = vec![TextSpan::lit(label)];
                }
            }
            None => {
                let child = world
                    .spawn((
                        UiNode,
                        Text {
                            spans: vec![TextSpan::lit(label)],
                            wrap: crate::text::WrapMode::None,
                            ..Default::default()
                        },
                    ))
                    .id();
                world.entity_mut(entity).add_child(child);
                world.resource_mut::<LayoutDirty>().mark();
            }
        }
    }
}

fn widget_buttons_toggles(world: &mut World) {
    let events: Vec<UiEvent> = world
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .cloned()
        .collect();
    let mut toggled: Vec<(Entity, bool)> = Vec::new();
    for event in &events {
        if event.kind != UiEventKind::Click {
            continue;
        }
        let target = event.target;
        if world.get::<Button>(target).is_some() {
            // Buttons already emit Click; gameplay listens for it directly.
            continue;
        }
        if let Some(toggle) = world.get::<Toggle>(target).cloned() {
            toggled.push((target, !toggle.checked));
        }
    }
    for (entity, checked) in toggled {
        if let Some(mut toggle) = world.get_mut::<Toggle>(entity) {
            toggle.checked = checked;
        }
        world.write_message(UiEvent::new(
            entity,
            UiEventKind::ValueChanged(UiValue::Bool(checked)),
        ));
    }
}

fn widget_sliders(world: &mut World) {
    let events: Vec<UiEvent> = world
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .cloned()
        .collect();
    let mut changed: Vec<(Entity, f32)> = Vec::new();
    for event in &events {
        let target = event.target;
        let Some(slider) = world.get::<Slider>(target).cloned() else {
            continue;
        };
        let Some(computed) = world.get::<ComputedNode>(target).copied() else {
            continue;
        };
        match &event.kind {
            UiEventKind::Drag(delta) => {
                let mut next = slider.value;
                let span = (slider.max - slider.min).abs();
                if span > 1e-6 {
                    next += delta[0] / computed.rect[2] * span;
                }
                changed.push((target, slider.value_near(next)));
            }
            UiEventKind::Click => {
                // Jump to the click position.
                if let Some(p) = world.resource::<UiPointer>().position() {
                    let fraction = ((p[0] - computed.rect[0]) / computed.rect[2].max(1.0)).clamp(0.0, 1.0);
                    changed.push((target, slider.value_at_fraction(fraction)));
                }
            }
            _ => {}
        }
    }
    for (entity, value) in changed {
        if let Some(mut slider) = world.get_mut::<Slider>(entity) {
            slider.value = value;
        }
        world.write_message(UiEvent::new(
            entity,
            UiEventKind::ValueChanged(UiValue::Number(value)),
        ));
        // No layout mark: slider values only move extract-drawn chrome (`Changed<Slider>` damages
        // extract), and a full taffy pass per drag delta is wasted work.
    }
}

/// The open option list of a dropdown (its popup root).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
struct DropdownList(Entity);

/// One option row of an open dropdown list.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
struct DropdownItem {
    dropdown: Entity,
    index: usize,
}

/// Type-ahead buffer of the focused dropdown (W04).
#[derive(Resource, Debug, Default)]
struct TypeAheadState {
    owner: Option<Entity>,
    buffer: String,
    idle: f32,
}

/// Seconds without typing after which type-ahead starts a new prefix.
const TYPE_AHEAD_RESET: f32 = 1.0;

/// Opens a dropdown's option list on the popup layer (below the dropdown, flipping above near
/// the bottom edge, at least as wide as the dropdown). Focus moves to the selected option.
/// Returns the popup root (the open one when already open).
pub(crate) fn open_dropdown_list(world: &mut World, dropdown: Entity) -> Option<Entity> {
    let dd = world.get::<Dropdown>(dropdown)?.clone();
    if let Some(list) = world.get::<DropdownList>(dropdown).map(|l| l.0)
        && world
            .get_resource::<crate::popup::PopupStack>()
            .is_some_and(|s| s.contains(list))
    {
        return Some(list);
    }
    let width = world
        .get::<ComputedNode>(dropdown)
        .map_or(0.0, |c| c.rect[2] / c.scale.max(0.001));
    let handle = crate::popup::open_popup(
        world,
        crate::popup::PopupRequest {
            kind: crate::popup::PopupKind::Dropdown,
            anchor: Some(dropdown),
            placement: crate::popup::PopupPlacement::Below,
            opener: Some(dropdown),
            min_width: width,
            classes: vec!["Menu".to_owned()],
            ..Default::default()
        },
    );
    let focus_index = dd.selected.filter(|i| *i < dd.options.len()).unwrap_or(0);
    for (index, option) in dd.options.iter().enumerate() {
        let style = crate::menu::row_style(world);
        let mut classes = vec!["MenuItem".to_owned()];
        if dd.selected == Some(index) {
            classes.push("MenuItem.checked".to_owned());
        }
        let row = world
            .spawn((
                UiNode,
                NodeStyle::new(style),
                Interactable::default(),
                crate::focus::Focusable::default(),
                crate::widget_style::WidgetSize::Sm,
                Classes(classes),
                DropdownItem { dropdown, index },
                Themed::default(),
                Text {
                    spans: vec![TextSpan::lit(option.clone())],
                    wrap: crate::text::WrapMode::None,
                    ..Default::default()
                },
                ChildOf(handle.panel),
            ))
            .id();
        if dd.selected == Some(index) {
            world.entity_mut(row).insert(crate::node::Selected);
        }
        if index == focus_index {
            world.entity_mut(row).insert(crate::focus::DefaultFocus);
        }
    }
    world.entity_mut(dropdown).insert(DropdownList(handle.root));
    if let Some(mut dd) = world.get_mut::<Dropdown>(dropdown) {
        dd.open = true;
    }
    Some(handle.root)
}

/// Closes a dropdown's option list if it is open. Returns the closed popups.
fn close_dropdown_list(world: &mut World, dropdown: Entity) -> Vec<Entity> {
    let closed = match world.get::<DropdownList>(dropdown).map(|l| l.0) {
        Some(list) => crate::popup::close_popup(world, list),
        None => Vec::new(),
    };
    if let Ok(mut entity) = world.get_entity_mut(dropdown) {
        entity.remove::<DropdownList>();
    }
    if let Some(mut dd) = world.get_mut::<Dropdown>(dropdown) {
        dd.open = false;
    }
    closed
}

/// Selects option `index` of a dropdown as a click on it does (closing the list, emitting
/// `ValueChanged(Index)`). Returns the closed popups.
pub(crate) fn select_dropdown(world: &mut World, dropdown: Entity, index: usize) -> Vec<Entity> {
    if let Some(mut dd) = world.get_mut::<Dropdown>(dropdown) {
        dd.selected = Some(index);
    }
    let closed = close_dropdown_list(world, dropdown);
    world.write_message(UiEvent::new(
        dropdown,
        UiEventKind::ValueChanged(UiValue::Index(index)),
    ));
    world.resource_mut::<LayoutDirty>().mark();
    closed
}

fn widget_dropdowns(world: &mut World, input: &crate::menu::FrameInput) {
    let events: Vec<UiEvent> = world
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .cloned()
        .collect();
    for event in &events {
        if event.kind != UiEventKind::Click {
            continue;
        }
        // A click (or Submit) on a dropdown toggles its list; a click on an option selects it.
        if let Some(item) = world.get::<DropdownItem>(event.target).copied() {
            select_dropdown(world, item.dropdown, item.index);
        } else if let Some(mut dd) = world.get_mut::<Dropdown>(event.target) {
            dd.open = !dd.open;
        }
    }
    // Keep each dropdown's list in step with `open`; a list closed by the popup layer (outside
    // press, Escape, gamepad B) clears `open`.
    let dropdowns: Vec<(Entity, bool, Option<Entity>)> = world
        .query::<(Entity, &Dropdown, Option<&DropdownList>)>()
        .iter(world)
        .map(|(e, d, l)| (e, d.open, l.map(|l| l.0)))
        .collect();
    for (entity, open, list) in dropdowns {
        let alive = list.is_some_and(|l| {
            world
                .get_resource::<crate::popup::PopupStack>()
                .is_some_and(|s| s.contains(l))
        });
        match (open, list.is_some(), alive) {
            (true, false, _) => {
                open_dropdown_list(world, entity);
            }
            (true, true, false) | (false, true, false) => {
                world.entity_mut(entity).remove::<DropdownList>();
                if let Some(mut dd) = world.get_mut::<Dropdown>(entity) {
                    dd.open = false;
                }
            }
            (false, true, true) => {
                close_dropdown_list(world, entity);
            }
            _ => {}
        }
    }
    dropdown_type_ahead(world, input);
}

/// Type-ahead (W04): letters typed while a dropdown (or one of its open options) has focus pick
/// the matching option: a closed dropdown selects it, an open list moves focus to it.
fn dropdown_type_ahead(world: &mut World, input: &crate::menu::FrameInput) {
    world.init_resource::<TypeAheadState>();
    let dt = world.get_resource::<Time>().map_or(0.0, |t| t.delta_secs());
    world.resource_mut::<TypeAheadState>().idle += dt;
    if input.chars.is_empty() {
        return;
    }
    let Some(focused) = world
        .get_resource::<crate::focus::FocusState>()
        .and_then(|f| f.focused)
    else {
        return;
    };
    let (dropdown, open_row) = if world.get::<Dropdown>(focused).is_some() {
        (focused, None)
    } else if let Some(item) = world.get::<DropdownItem>(focused).copied() {
        (item.dropdown, Some(item.index))
    } else {
        return;
    };
    let Some(dd) = world.get::<Dropdown>(dropdown).cloned() else {
        return;
    };
    let buffer = {
        let mut state = world.resource_mut::<TypeAheadState>();
        if state.owner != Some(dropdown) || state.idle > TYPE_AHEAD_RESET {
            state.buffer.clear();
        }
        state.owner = Some(dropdown);
        state.idle = 0.0;
        state.buffer.extend(input.chars.iter());
        state.buffer.clone()
    };
    let current = open_row.or(dd.selected);
    let Some(index) = crate::menu::type_ahead(&dd.options, current, &buffer) else {
        return;
    };
    if open_row.is_some() {
        let row = world
            .query::<(Entity, &DropdownItem)>()
            .iter(world)
            .find(|(_, item)| item.dropdown == dropdown && item.index == index)
            .map(|(e, _)| e);
        if let Some(row) = row {
            crate::popup::set_focus(world, Some(row));
        }
    } else if dd.selected != Some(index) {
        if let Some(mut dd) = world.get_mut::<Dropdown>(dropdown) {
            dd.selected = Some(index);
        }
        world.write_message(UiEvent::new(
            dropdown,
            UiEventKind::ValueChanged(UiValue::Index(index)),
        ));
        world.resource_mut::<LayoutDirty>().mark();
    }
}

fn widget_tab_bars(world: &mut World) {
    let events: Vec<UiEvent> = world
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .cloned()
        .collect();
    for event in &events {
        if event.kind != UiEventKind::Click {
            continue;
        }
        let target = event.target;
        // Tab buttons are children of the tab bar carrying TabIndex.
        if let Some(index) = world.get::<TabIndex>(target).cloned() {
            let parent = world.get::<ChildOf>(target).map(|c| c.0);
            let Some(parent) = parent else {
                continue;
            };
            if let Some(mut tabs) = world.get_mut::<TabBar>(parent)
                && tabs.active != index.0
            {
                tabs.active = index.0;
                world.write_message(UiEvent::new(
                    parent,
                    UiEventKind::ValueChanged(UiValue::Index(index.0)),
                ));
            }
        }
    }
}

/// The tab index of a tab button.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TabIndex(pub usize);

fn widget_modals(world: &mut World) {
    let events: Vec<UiEvent> = world
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .cloned()
        .collect();
    for event in &events {
        if event.kind != UiEventKind::Click {
            continue;
        }
        if let Some(mut modal) = world.get_mut::<Modal>(event.target)
            && modal.open
            && modal.close_on_cancel
        {
            modal.open = false;
        }
    }
}

/// Opens a tooltip's popup at once and keeps it open until [`unpin_tooltip`] (agents). Returns
/// the popup root.
pub(crate) fn pin_tooltip(world: &mut World, host: Entity) -> Option<Entity> {
    {
        let mut tooltip = world.get_mut::<Tooltip>(host)?;
        tooltip.pinned = true;
        tooltip.visible = true;
    }
    sync_tooltip_popup(world, host)
}

/// Releases a pinned tooltip (it hides unless hovered or focused).
pub(crate) fn unpin_tooltip(world: &mut World, host: Entity) {
    if let Some(mut tooltip) = world.get_mut::<Tooltip>(host) {
        tooltip.pinned = false;
        tooltip.visible = false;
        tooltip.elapsed = 0.0;
        tooltip.popup = None;
    }
}

/// The text a tooltip shows: the hint machine's text while it shows `host` (glyph placeholders
/// expanded, or the navigation text), else the authored [`Tooltip::text`]. The shown text is
/// never written back into the component, so the authored text and its placeholders survive.
fn tooltip_shown_text(world: &World, host: Entity) -> Option<&str> {
    world
        .get_resource::<crate::hints::TooltipState>()
        .filter(|state| state.shown_entity() == Some(host))
        .and_then(crate::hints::TooltipState::text)
        .or_else(|| world.get::<Tooltip>(host).map(|tooltip| tooltip.text.as_str()))
}

/// The literal text of a label's first span, if it is a literal.
fn first_lit(text: &Text) -> Option<&str> {
    text.spans.first().and_then(|span| match &span.content {
        crate::text::TextContent::Lit(lit) => Some(lit.as_str()),
        crate::text::TextContent::Loc { .. } => None,
    })
}

/// Counts one widget label string written (one `String` allocation).
fn count_widget_text_clone(world: &mut World) {
    if let Some(mut counts) = world.get_resource_mut::<crate::layout::UiHotPathCounts>() {
        counts.widget_text_clones += 1;
    }
}

/// Opens or closes a tooltip's popup to match `visible`; returns the open popup.
fn sync_tooltip_popup(world: &mut World, host: Entity) -> Option<Entity> {
    let (visible, popup) = world
        .get::<Tooltip>(host)
        .map(|tooltip| (tooltip.visible, tooltip.popup))?;
    if !visible && popup.is_none() {
        return None;
    }
    let alive = popup.filter(|p| {
        world
            .get_resource::<crate::popup::PopupStack>()
            .is_some_and(|s| s.contains(*p))
    });
    match (visible, alive) {
        (true, Some(popup)) => {
            // Follow text changes, comparing in place: an idle open tooltip allocates nothing.
            let panel = world.get::<crate::popup::Popup>(popup).map(|p| p.panel);
            let label = panel
                .and_then(|panel| world.get::<Children>(panel))
                .and_then(|c| c.iter().find(|e| world.get::<Text>(*e).is_some()));
            if let Some(label) = label {
                let stale = world
                    .get::<Text>(label)
                    .is_some_and(|text| first_lit(text) != tooltip_shown_text(world, host));
                if stale && let Some(shown) = tooltip_shown_text(world, host).map(str::to_owned) {
                    count_widget_text_clone(world);
                    if let Some(mut text) = world.get_mut::<Text>(label) {
                        text.spans = vec![TextSpan::lit(shown)];
                    }
                }
            }
            Some(popup)
        }
        (true, None) => {
            let handle = crate::popup::open_popup(
                world,
                crate::popup::PopupRequest {
                    kind: crate::popup::PopupKind::Tooltip,
                    anchor: Some(host),
                    placement: crate::popup::PopupPlacement::Below,
                    interactive: false,
                    classes: vec!["Tooltip".to_owned()],
                    ..Default::default()
                },
            );
            if let Some(mut style) = world.get_mut::<NodeStyle>(handle.panel) {
                style.0.max_size = [Dimension::Points(320.0), Dimension::Auto];
                style.0.padding = Edges {
                    left: Dimension::Points(8.0),
                    top: Dimension::Points(4.0),
                    right: Dimension::Points(8.0),
                    bottom: Dimension::Points(4.0),
                };
            }
            let shown = tooltip_shown_text(world, host).unwrap_or_default().to_owned();
            count_widget_text_clone(world);
            world.spawn((UiNode, Text::new(shown), ChildOf(handle.panel)));
            if let Some(mut t) = world.get_mut::<Tooltip>(host) {
                t.popup = Some(handle.root);
            }
            Some(handle.root)
        }
        (false, Some(popup)) => {
            crate::popup::close_popup(world, popup);
            if let Some(mut t) = world.get_mut::<Tooltip>(host) {
                t.popup = None;
            }
            None
        }
        (false, None) => {
            if popup.is_some()
                && let Some(mut t) = world.get_mut::<Tooltip>(host)
            {
                t.popup = None;
            }
            None
        }
    }
}

fn widget_tooltips(world: &mut World) {
    // Timing, warm-up and dismissal live in the hint machine (hints.rs); it sets `visible`. An
    // agent pin (`ui.open_popup`) keeps a tooltip open whatever the machine says.
    crate::hints::update_tooltips(world);
    let tooltips: Vec<Entity> = world
        .query_filtered::<Entity, With<Tooltip>>()
        .iter(world)
        .collect();
    for entity in tooltips {
        if let Some(mut tooltip) = world.get_mut::<Tooltip>(entity)
            && tooltip.pinned
            && !tooltip.visible
        {
            tooltip.visible = true;
        }
        sync_tooltip_popup(world, entity);
    }
}

fn widget_virtual_controls(world: &mut World) {
    let sticks: Vec<Entity> = world
        .query::<(Entity, &VirtualStick)>()
        .iter(world)
        .map(|(e, _)| e)
        .collect();
    let button_entities: Vec<Entity> = world
        .query::<(Entity, &VirtualButton)>()
        .iter(world)
        .filter(|(_, button)| !button.action.is_empty())
        .map(|(entity, _)| entity)
        .collect();
    if sticks.is_empty() && button_entities.is_empty() {
        // No controls this frame. Clear a stale map, and leave an empty one untouched so its
        // change tick stays put.
        if world
            .get_resource::<VirtualAxes>()
            .is_some_and(|axes| !axes.0.is_empty())
        {
            world.resource_mut::<VirtualAxes>().0.clear();
        }
        if world
            .get_resource::<VirtualButtons>()
            .is_some_and(|buttons| !buttons.0.is_empty())
        {
            world.resource_mut::<VirtualButtons>().0.clear();
        }
        // Release the holds of controls that are gone (spec 11 § 7); with none held, the input
        // layer is left untouched too.
        if world
            .get_resource::<VirtualControlHolds>()
            .is_some_and(|holds| !holds.0.is_empty())
        {
            inject_virtual_controls(
                world,
                &std::collections::BTreeMap::new(),
                &std::collections::HashMap::new(),
            );
        }
        return;
    }
    let pointer = world.get_resource::<UiPointer>().cloned().unwrap_or_default();
    let mut axes = world
        .get_resource::<VirtualAxes>()
        .map(|a| a.0.clone())
        .unwrap_or_default();
    // Query order follows archetypes; walk in entity order so duplicates and touch claims are
    // resolved the same way on every run.
    let mut sticks = sticks;
    sticks.sort_by_key(|e| e.to_bits());
    let mut claimed_touches = Vec::new();
    let mut axes_written: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Sticks under a finger, by action (the more deflected one when two share an action).
    let mut touched: std::collections::BTreeMap<String, [f32; 2]> = std::collections::BTreeMap::new();
    for entity in sticks {
        let Some(computed) = world.get::<ComputedNode>(entity).copied() else {
            continue;
        };
        let Some(mut stick) = world.get_mut::<VirtualStick>(entity) else {
            continue;
        };
        let center = [
            computed.rect[0] + computed.rect[2] / 2.0,
            computed.rect[1] + computed.rect[3] / 2.0,
        ];
        let radius = stick.radius * computed.scale;
        if let Some(touch_id) = stick.touch {
            match pointer.touches.get(&touch_id) {
                Some(p) => {
                    let dx = (p[0] - center[0]) / radius.max(1.0);
                    let dy = -(p[1] - center[1]) / radius.max(1.0);
                    let len = (dx * dx + dy * dy).sqrt();
                    let (dx, dy) = if len > 1.0 { (dx / len, dy / len) } else { (dx, dy) };
                    stick.value = [dx, dy];
                }
                None => {
                    stick.touch = None;
                    stick.value = [0.0; 2];
                }
            }
        } else if let Some(p) = pointer.position()
            && pointer.active_touch.is_some()
            && p[0] >= computed.rect[0]
            && p[0] <= computed.rect[0] + computed.rect[2]
            && p[1] >= computed.rect[1]
            && p[1] <= computed.rect[1] + computed.rect[3]
            && let Some(id) = pointer.active_touch
            && !claimed_touches.contains(&id)
        {
            stick.touch = Some(id);
        }
        if let Some(id) = stick.touch {
            claimed_touches.push(id);
        }
        let mag = |v: [f32; 2]| v[0] * v[0] + v[1] * v[1];
        if stick.touch.is_some() && !stick.action.is_empty() {
            let value = stick.value;
            touched
                .entry(stick.action.clone())
                .and_modify(|old: &mut [f32; 2]| {
                    if mag(value) > mag(*old) {
                        *old = value;
                    }
                })
                .or_insert(value);
        }
        if !stick.action.is_empty() {
            let value = stick.value;
            // Two sticks bound to one action: the more deflected one is published.
            if !axes_written.insert(stick.action.clone())
                && axes.get(&stick.action).is_some_and(|old| mag(*old) >= mag(value))
            {
                continue;
            }
            axes.insert(stick.action.clone(), value);
        }
    }
    let axes_changed = world
        .get_resource::<VirtualAxes>()
        .is_none_or(|current| current.0 != axes);
    if axes_changed {
        world.resource_mut::<VirtualAxes>().0 = axes;
    }
    // Virtual buttons from Hovered+Pressed state.
    let vbuttons: Vec<(Entity, String)> = button_entities
        .iter()
        .filter_map(|entity| {
            world
                .get::<VirtualButton>(*entity)
                .filter(|button| !button.action.is_empty())
                .map(|button| (*entity, button.action.clone()))
        })
        .collect();
    let mut buttons = std::collections::HashMap::new();
    for (entity, action) in vbuttons {
        let pressed = world.get::<crate::node::Pressed>(entity).is_some();
        // Two buttons bound to one action: held if either is held.
        *buttons.entry(action).or_insert(false) |= pressed;
    }
    inject_virtual_controls(world, &touched, &buttons);
    let buttons_changed = world
        .get_resource::<VirtualButtons>()
        .is_none_or(|current| current.0 != buttons);
    if buttons_changed {
        world.resource_mut::<VirtualButtons>().0 = buttons;
    }
}

/// The actions [`widget_virtual_controls`] holds in [`regia_input::InputInjection`], so it releases
/// only its own holds (agent schedules and `TouchControls` zones share the resource).
#[derive(Resource, Debug, Default)]
struct VirtualControlHolds(std::collections::BTreeSet<String>);

/// Writes the virtual controls to the action layer (spec 11 § 7): a touched stick holds its action
/// at its deflection (x right, y up) and a pressed button holds its action at `1.0`, through
/// [`regia_input::InputInjection::hold`]; a lifted finger releases it, so the action reads its
/// device bindings again. A stick wins over a button bound to the same action. Does nothing when
/// the app has no input layer (no `InputInjection` resource).
fn inject_virtual_controls(
    world: &mut World,
    touched: &std::collections::BTreeMap<String, [f32; 2]>,
    buttons: &std::collections::HashMap<String, bool>,
) {
    use regia_core::math::Vec3;
    if !world.contains_resource::<regia_input::InputInjection>() {
        return;
    }
    let mut now: std::collections::BTreeMap<String, Vec3> = buttons
        .iter()
        .filter(|(_, pressed)| **pressed)
        .map(|(action, _)| (action.clone(), Vec3::X))
        .collect();
    for (action, v) in touched {
        now.insert(action.clone(), Vec3::new(v[0], v[1], 0.0));
    }
    let before = std::mem::take(&mut world.get_resource_or_init::<VirtualControlHolds>().0);
    let mut injection = world.resource_mut::<regia_input::InputInjection>();
    for action in before.iter().filter(|a| !now.contains_key(*a)) {
        injection.release(action);
    }
    for (action, value) in &now {
        injection.hold(action.clone(), *value);
    }
    world.resource_mut::<VirtualControlHolds>().0 = now.into_keys().collect();
}

/// Spawns a standard button (label + classes) under `parent`.
pub fn spawn_button(commands: &mut Commands, parent: Entity, label: &str, classes: &[&str]) -> Entity {
    let mut bundle_classes = vec!["button".to_owned()];
    bundle_classes.extend(classes.iter().map(|c| (*c).to_owned()));
    let entity = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                padding: Edges::px(10.0),
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                ..Default::default()
            }),
            Interactable::default(),
            crate::focus::Focusable::default(),
            Button,
            Classes(bundle_classes),
            Themed::default(),
        ))
        .with_children(|b| {
            b.spawn((
                UiNode,
                Text {
                    spans: vec![TextSpan::lit(label.to_owned())],
                    wrap: crate::text::WrapMode::None,
                    ..Default::default()
                },
            ));
        })
        .id();
    commands.entity(parent).add_child(entity);
    entity
}

/// A panel helper: a bordered surface with padding.
pub fn panel_style() -> Style {
    Style {
        direction: FlexDirection::Column,
        padding: Edges::px(16.0),
        gap: [Dimension::Points(8.0), Dimension::Points(8.0)],
        ..Default::default()
    }
}

/// Hides a node subtree (display: none).
pub fn set_hidden(node: &mut NodeStyle) {
    node.0.display = Display::None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Component)]
    struct Marker;

    #[test]
    fn idle_dropdown_and_tooltip_do_not_clone_their_text() {
        let mut world = World::new();
        world.init_resource::<crate::layout::LayoutDirty>();
        world.init_resource::<crate::layout::UiHotPathCounts>();
        world.init_resource::<crate::popup::PopupStack>();
        let dropdown = world
            .spawn(Dropdown {
                options: (0..200).map(|i| format!("option {i}")).collect(),
                selected: Some(3),
                open: false,
            })
            .id();
        let tabs = world
            .spawn(TabBar {
                tabs: vec!["One".into(), "Two".into(), "Three".into()],
                active: 0,
            })
            .id();
        let tip = world.spawn(Tooltip::new("row hint")).id();
        // A second tooltip, open (pinned), so the visible path is idle-synced too.
        let open_tip = world.spawn(Tooltip::new("open hint")).id();
        assert!(
            pin_tooltip(&mut world, open_tip).is_some(),
            "pinned tooltip opened"
        );
        sync_dropdown_labels(&mut world);
        sync_tab_bar_children(&mut world);
        sync_tooltip_popup(&mut world, tip);
        sync_tooltip_popup(&mut world, open_tip);
        let label = world
            .get::<Children>(dropdown)
            .and_then(|c| c.iter().find(|e| world.get::<Text>(*e).is_some()))
            .expect("dropdown label");
        let tick = world.entity(label).get_change_ticks::<Text>().unwrap().changed;
        let before = world
            .resource::<crate::layout::UiHotPathCounts>()
            .widget_text_clones;
        for _ in 0..5 {
            sync_dropdown_labels(&mut world);
            sync_tab_bar_children(&mut world);
            sync_tooltip_popup(&mut world, tip);
            sync_tooltip_popup(&mut world, open_tip);
        }
        let after = world
            .resource::<crate::layout::UiHotPathCounts>()
            .widget_text_clones;
        assert_eq!(
            world.entity(label).get_change_ticks::<Text>().unwrap().changed,
            tick,
            "the dropdown label was rewritten while idle"
        );
        assert_eq!(
            after,
            before,
            "idle sync cloned widget text {} times",
            after - before
        );
        let _ = tabs;
    }

    #[test]
    fn duplicate_virtual_button_actions_are_held_if_either_is_held() {
        for marker_on_pressed in [false, true] {
            let mut world = World::new();
            world.init_resource::<UiPointer>();
            world.init_resource::<VirtualAxes>();
            world.init_resource::<VirtualButtons>();
            let held = world
                .spawn((
                    VirtualButton {
                        action: "confirm".into(),
                    },
                    crate::node::Pressed,
                ))
                .id();
            let idle = world
                .spawn(VirtualButton {
                    action: "confirm".into(),
                })
                .id();
            let marked = if marker_on_pressed { held } else { idle };
            world.entity_mut(marked).insert(Marker);
            widget_virtual_controls(&mut world);
            assert_eq!(world.resource::<VirtualButtons>().0.get("confirm"), Some(&true));
        }
        let mut idle = World::new();
        idle.init_resource::<UiPointer>();
        idle.init_resource::<VirtualAxes>();
        idle.init_resource::<VirtualButtons>();
        widget_virtual_controls(&mut idle);
        let axes_tick = idle
            .get_resource_change_ticks::<VirtualAxes>()
            .expect("axes ticks")
            .changed;
        let buttons_tick = idle
            .get_resource_change_ticks::<VirtualButtons>()
            .expect("button ticks")
            .changed;
        widget_virtual_controls(&mut idle);
        assert_eq!(
            idle.get_resource_change_ticks::<VirtualAxes>()
                .expect("axes ticks")
                .changed,
            axes_tick
        );
        assert_eq!(
            idle.get_resource_change_ticks::<VirtualButtons>()
                .expect("button ticks")
                .changed,
            buttons_tick
        );
    }

    fn virtual_world() -> World {
        let mut world = World::new();
        world.init_resource::<UiPointer>();
        world.init_resource::<VirtualAxes>();
        world.init_resource::<VirtualButtons>();
        world.init_resource::<regia_input::InputInjection>();
        world
    }

    fn injected(world: &World, action: &str) -> Option<regia_core::math::Vec3> {
        world
            .resource::<regia_input::InputInjection>()
            .value_for(action, 0)
    }

    /// spec 11 § 7 (spec conformance 03-11): a `VirtualButton` writes to its action. Before the
    /// fix it only filled `VirtualButtons`, which nothing in the input layer reads, so
    /// `actions.pressed("Jump")` never saw the touch button.
    #[test]
    fn a_pressed_virtual_button_holds_its_action_and_releases_on_lift() {
        use regia_core::math::Vec3;
        let mut world = virtual_world();
        // Someone else's hold (an agent, a touch zone) must survive the widget's release.
        world
            .resource_mut::<regia_input::InputInjection>()
            .hold("Other", Vec3::Y);
        let button = world
            .spawn((
                VirtualButton {
                    action: "Jump".into(),
                },
                crate::node::Pressed,
            ))
            .id();
        widget_virtual_controls(&mut world);
        assert_eq!(injected(&world, "Jump"), Some(Vec3::X));
        world.entity_mut(button).remove::<crate::node::Pressed>();
        widget_virtual_controls(&mut world);
        assert_eq!(injected(&world, "Jump"), None, "released on lift");
        assert_eq!(injected(&world, "Other"), Some(Vec3::Y), "only its own holds");
        world
            .resource_mut::<regia_input::InputInjection>()
            .hold("Jump", Vec3::Z);
        widget_virtual_controls(&mut world);
        assert_eq!(
            injected(&world, "Jump"),
            Some(Vec3::Z),
            "an idle widget does not release a hold it no longer owns"
        );
    }

    /// spec 11 § 7: a touched `VirtualStick` holds its action at the deflection (x right, y up)
    /// and releases it when the finger lifts.
    #[test]
    fn a_touched_virtual_stick_holds_its_deflection_and_releases_on_lift() {
        use regia_core::math::Vec3;
        let mut world = virtual_world();
        world.spawn((
            VirtualStick {
                action: "Move".into(),
                radius: 50.0,
                touch: Some(7),
                ..Default::default()
            },
            ComputedNode {
                rect: [0.0, 0.0, 100.0, 100.0],
                scale: 1.0,
                ..Default::default()
            },
        ));
        // The finger 25 px right of and 25 px above the center.
        world.resource_mut::<UiPointer>().touches.insert(7, [75.0, 25.0]);
        widget_virtual_controls(&mut world);
        assert_eq!(injected(&world, "Move"), Some(Vec3::new(0.5, 0.5, 0.0)));
        assert_eq!(world.resource::<VirtualAxes>().0["Move"], [0.5, 0.5]);
        world.resource_mut::<UiPointer>().touches.clear();
        widget_virtual_controls(&mut world);
        assert_eq!(injected(&world, "Move"), None, "released on lift");
        assert_eq!(world.resource::<VirtualAxes>().0["Move"], [0.0, 0.0]);
    }

    /// Without an input layer the widgets keep working on their own resources.
    #[test]
    fn virtual_controls_without_an_input_layer_only_fill_their_resources() {
        let mut world = World::new();
        world.init_resource::<UiPointer>();
        world.init_resource::<VirtualAxes>();
        world.init_resource::<VirtualButtons>();
        world.spawn((
            VirtualButton {
                action: "Jump".into(),
            },
            crate::node::Pressed,
        ));
        widget_virtual_controls(&mut world);
        assert_eq!(world.resource::<VirtualButtons>().0.get("Jump"), Some(&true));
        assert!(!world.contains_resource::<regia_input::InputInjection>());
    }

    /// audit-seq (Medium): a drag (keyboard and gamepad steps are drags too) on a reversed slider
    /// measured its fraction from `min` and landed on the low end.
    #[test]
    fn a_drag_on_a_reversed_slider_moves_from_its_value() {
        let mut world = World::new();
        world.init_resource::<Messages<UiEvent>>();
        let entity = world
            .spawn((
                Slider {
                    value: 5.0,
                    min: 10.0,
                    max: 0.0,
                    step: 1.0,
                },
                ComputedNode {
                    rect: [0.0, 0.0, 100.0, 10.0],
                    ..Default::default()
                },
            ))
            .id();
        // 10 % of the width is 10 % of the span.
        world.write_message(UiEvent::new(entity, UiEventKind::Drag([10.0, 0.0])));
        widget_sliders(&mut world);
        assert_eq!(world.get::<Slider>(entity).unwrap().value, 6.0);
    }

    #[test]
    fn slider_fraction_roundtrip() {
        let s = Slider {
            value: 0.5,
            min: 0.0,
            max: 10.0,
            step: 0.0,
        };
        assert!((s.fraction() - 0.05).abs() < 1e-5);
        assert_eq!(s.value_at_fraction(0.5), 5.0);
        let stepped = Slider {
            value: 0.0,
            min: 0.0,
            max: 1.0,
            step: 0.25,
        };
        assert_eq!(stepped.value_at_fraction(0.3), 0.25);
        assert_eq!(stepped.value_at_fraction(0.9), 1.0);
    }

    #[test]
    fn slider_value_never_non_finite() {
        let inf_step = Slider {
            value: 0.0,
            min: 0.0,
            max: 1.0,
            step: f32::INFINITY,
        };
        assert_eq!(inf_step.value_at_fraction(0.5), 0.5);
        assert_eq!(inf_step.value_at_fraction(f32::NAN), 0.0);
        let bad_range = Slider {
            value: 0.0,
            min: 0.0,
            max: f32::INFINITY,
            step: 0.0,
        };
        assert_eq!(bad_range.value_at_fraction(0.5), 0.0);
    }

    #[test]
    fn slider_value_at_fraction_survives_bad_ends() {
        let reversed = Slider {
            value: 0.0,
            min: 10.0,
            max: 0.0,
            step: 0.0,
        };
        assert_eq!(reversed.value_at_fraction(0.5), 5.0);
        let nan = Slider {
            value: 0.0,
            min: f32::NAN,
            max: f32::NAN,
            step: 0.0,
        };
        assert_eq!(nan.value_at_fraction(0.5), 0.0);
        let inf = Slider {
            value: 0.0,
            min: 0.0,
            max: f32::INFINITY,
            step: 0.0,
        };
        assert_eq!(inf.value_at_fraction(0.5), 0.0);
    }

    #[test]
    fn text_input_stale_caret_does_not_panic() {
        let mut input = TextInput {
            text: "日本".to_owned(),
            caret: 99,
            selection_anchor: 4, // inside the second character
            ..Default::default()
        };
        assert!(input.replace_selection("x", crate::text_edit::EditKind::Typing));
        assert_eq!(input.text, "日x");
        assert_eq!(input.caret, input.text.len());
        assert_eq!(input.selection_anchor, input.caret);
        // Caret in the middle of a character snaps back.
        let mut mid = TextInput {
            text: "日".to_owned(),
            caret: 1,
            selection_anchor: 1,
            ..Default::default()
        };
        assert!(mid.replace_selection("a", crate::text_edit::EditKind::Typing));
        assert_eq!(mid.text, "a日");
    }
}
