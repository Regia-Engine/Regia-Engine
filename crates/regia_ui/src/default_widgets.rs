// SPDX-License-Identifier: MPL-2.0
//! The default widgets of ADR-273 D11: [`RadioButton`], [`SpinBox`], [`Foldable`], [`Split`] and
//! [`Toast`] (with its [`ToastQueue`] and [`ToastHost`]), plus the ADR-273 W11 path navigator
//! [`Breadcrumbs`] (with its [`CrumbIndex`] crumb marker).
//!
//! They follow the rules of the older widgets in [`crate::widgets`]: plain components, reaction to
//! [`UiEvent`]s inside [`crate::widgets::ui_widgets`], `ValueChanged` events for state changes, and
//! the theme `Selected` marker for the "on" look. Every state change goes through one `pub(crate)`
//! helper (`select_radio`, `step_spin_box`, `set_fold`, `drag_split`, `tick_toasts`,
//! `navigate_breadcrumbs`), so the pointer path, the agent bridge (`ui.set_value`) and the tests
//! share the same code. The tree widget of the same roadmap row is
//! [`TreeView`](crate::parity_widgets::TreeView), on the virtualised list.

use std::collections::VecDeque;

use regia_app::prelude::*;

use crate::interaction::{UiEvent, UiEventKind, UiPointer, UiValue};
use crate::layout::LayoutDirty;
use crate::node::{Classes, ComputedNode, Interactable, NodeStyle, Selected, UiNode};
use crate::style::{Align, Dimension, Display, Edges, FlexDirection, JustifyContent, Style};
use crate::text::{Text, TextContent, TextSpan, WrapMode};
use crate::theme::Themed;

/// Most requests a [`ToastQueue`] keeps; the oldest are dropped past this.
const MAX_QUEUED_TOASTS: usize = 64;

/// One option of an exclusive group. Clicking (or `Submit` while focused) checks it and unchecks
/// the other radio buttons with the same `group` under the same parent. Emits `ValueChanged(Bool)`
/// for every button whose state changed. A checked button also carries [`Selected`], so themes
/// draw the "on" look.
#[derive(Component, Debug, Clone, PartialEq, Eq, Default)]
pub struct RadioButton {
    /// Group name; buttons are exclusive with others of the same name under the same parent.
    pub group: String,
    /// Whether this button is the checked one.
    pub checked: bool,
}

/// Checks `entity`'s radio button and unchecks the rest of its group. Returns whether anything
/// changed (a checked button, a disabled button or a non-radio changes nothing).
pub(crate) fn select_radio(world: &mut World, entity: Entity) -> bool {
    let Some(radio) = world.get::<RadioButton>(entity) else {
        return false;
    };
    if radio.checked || crate::node::is_disabled(world, entity) {
        return false;
    }
    let group = radio.group.clone();
    let parent = world.get::<ChildOf>(entity).map(|c| c.0);
    let others: Vec<Entity> = world
        .query::<(Entity, &RadioButton)>()
        .iter(world)
        .filter(|(e, r)| *e != entity && r.checked && r.group == group)
        .map(|(e, _)| e)
        .filter(|e| world.get::<ChildOf>(*e).map(|c| c.0) == parent)
        .collect();
    for other in others {
        if let Some(mut r) = world.get_mut::<RadioButton>(other) {
            r.checked = false;
        }
        world.entity_mut(other).remove::<Selected>();
        world.write_message(UiEvent::new(
            other,
            UiEventKind::ValueChanged(UiValue::Bool(false)),
        ));
    }
    if let Some(mut r) = world.get_mut::<RadioButton>(entity) {
        r.checked = true;
    }
    world.entity_mut(entity).insert(Selected);
    world.write_message(UiEvent::new(
        entity,
        UiEventKind::ValueChanged(UiValue::Bool(true)),
    ));
    true
}

/// A number field: drag sideways, scroll the wheel, or click the left or right edge to change
/// the value by `step`. Shows `prefix`, the value with `decimals` digits, then `suffix` in its
/// first `Text` child (one is created when missing). Emits `ValueChanged(Number)`.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct SpinBox {
    /// Current value.
    pub value: f32,
    /// Minimum.
    pub min: f32,
    /// Maximum.
    pub max: f32,
    /// Step (0 = continuous; drag and wheel then move by `0.1` of the range per step).
    pub step: f32,
    /// Digits after the decimal point shown (0 to 6).
    pub decimals: u8,
    /// Text shown before the number.
    pub prefix: String,
    /// Text shown after the number (a unit).
    pub suffix: String,
    /// Horizontal drag distance in UI px that moves the value by one step.
    pub px_per_step: f32,
    /// Drag distance not yet turned into whole steps.
    pub(crate) drag_accum: f32,
}

impl Default for SpinBox {
    fn default() -> Self {
        Self {
            value: 0.0,
            min: 0.0,
            max: 100.0,
            step: 1.0,
            decimals: 0,
            prefix: String::new(),
            suffix: String::new(),
            px_per_step: 6.0,
            drag_accum: 0.0,
        }
    }
}

impl SpinBox {
    /// The effective step: `step`, or a tenth of the range when `step` is not positive.
    fn effective_step(&self) -> f32 {
        if self.step > 0.0 && self.step.is_finite() {
            self.step
        } else {
            ((self.max - self.min).abs() * 0.1).max(f32::EPSILON)
        }
    }

    /// `v` snapped to the step grid (anchored at `min`, only when `step` is positive), rounded to
    /// `decimals` digits and clamped to the range. A non-finite `v` returns the current value.
    pub fn snap(&self, v: f32) -> f32 {
        if !v.is_finite() {
            return self.value;
        }
        let lo = self.min.min(self.max);
        let hi = self.min.max(self.max);
        let mut v = v;
        if self.step > 0.0 && self.step.is_finite() {
            v = lo + ((v - lo) / self.step).round() * self.step;
        }
        let factor = 10f32.powi(i32::from(self.decimals.min(6)));
        v = (v * factor).round() / factor;
        v.max(lo).min(hi)
    }

    /// The value after `steps` steps up (negative = down), snapped.
    pub fn stepped(&self, steps: i32) -> f32 {
        self.snap(self.value + steps as f32 * self.effective_step())
    }

    /// The label text: prefix, number, suffix.
    pub fn display(&self) -> String {
        let digits = usize::from(self.decimals.min(6));
        format!("{}{:.digits$}{}", self.prefix, self.value, self.suffix)
    }
}

/// Moves a spin box by `steps` (negative = down). Returns the new value when it changed.
pub(crate) fn step_spin_box(world: &mut World, entity: Entity, steps: i32) -> Option<f32> {
    if crate::node::is_disabled(world, entity) {
        return None;
    }
    let mut spin = world.get_mut::<SpinBox>(entity)?;
    let next = spin.stepped(steps);
    if (next - spin.value).abs() <= f32::EPSILON {
        return None;
    }
    spin.value = next;
    world.write_message(UiEvent::new(
        entity,
        UiEventKind::ValueChanged(UiValue::Number(next)),
    ));
    Some(next)
}

/// Sets a spin box's value (snapped and clamped). Returns the stored value when it changed.
pub(crate) fn set_spin_box(world: &mut World, entity: Entity, value: f32) -> Option<f32> {
    let mut spin = world.get_mut::<SpinBox>(entity)?;
    let next = spin.snap(value);
    if (next - spin.value).abs() <= f32::EPSILON {
        return None;
    }
    spin.value = next;
    world.write_message(UiEvent::new(
        entity,
        UiEventKind::ValueChanged(UiValue::Number(next)),
    ));
    Some(next)
}

/// A collapsible section. Its [`FoldHeader`] child toggles it; its [`FoldBody`] children are
/// hidden (`display: none`) while closed. Foldables with the same `group` under the same parent
/// are exclusive (an accordion): opening one closes the others. Emits `ValueChanged(Bool)` with
/// the new open state.
#[derive(Component, Debug, Clone, PartialEq, Eq, Default)]
pub struct Foldable {
    /// Whether the body is shown.
    pub open: bool,
    /// Accordion group; `None` = independent.
    pub group: Option<String>,
}

/// Marks the clickable header child of a [`Foldable`].
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FoldHeader;

/// Marks a child of a [`Foldable`] that is shown only while it is open.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldBody {
    /// The `display` value while open (what the node had before it was ever hidden).
    pub display: Display,
}

impl Default for FoldBody {
    fn default() -> Self {
        Self {
            display: Display::Flex,
        }
    }
}

/// Opens or closes a foldable and closes its group mates when opening. Returns whether the state
/// changed.
pub(crate) fn set_fold(world: &mut World, entity: Entity, open: bool) -> bool {
    let Some(fold) = world.get::<Foldable>(entity) else {
        return false;
    };
    if fold.open == open {
        return false;
    }
    let group = fold.group.clone();
    if let Some(mut fold) = world.get_mut::<Foldable>(entity) {
        fold.open = open;
    }
    world.write_message(UiEvent::new(
        entity,
        UiEventKind::ValueChanged(UiValue::Bool(open)),
    ));
    if open && let Some(group) = group {
        let parent = world.get::<ChildOf>(entity).map(|c| c.0);
        let mates: Vec<Entity> = world
            .query::<(Entity, &Foldable)>()
            .iter(world)
            .filter(|(e, f)| *e != entity && f.open && f.group.as_deref() == Some(group.as_str()))
            .map(|(e, _)| e)
            .filter(|e| world.get::<ChildOf>(*e).map(|c| c.0) == parent)
            .collect();
        for mate in mates {
            if let Some(mut f) = world.get_mut::<Foldable>(mate) {
                f.open = false;
            }
            world.write_message(UiEvent::new(
                mate,
                UiEventKind::ValueChanged(UiValue::Bool(false)),
            ));
        }
    }
    true
}

/// Which way a [`Split`] lays its two panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SplitAxis {
    /// Panes side by side; the divider moves left and right.
    #[default]
    Horizontal,
    /// Panes stacked; the divider moves up and down.
    Vertical,
}

/// Two panes with a draggable divider. Children, in order: first pane, a [`SplitDivider`], second
/// pane. The panes share the space left by the divider in the ratio `ratio : 1 - ratio`. Emits
/// `ValueChanged(Number)` with the new ratio.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Split {
    /// Pane layout.
    pub axis: SplitAxis,
    /// Share of the first pane, `min_ratio..=max_ratio`.
    pub ratio: f32,
    /// Smallest allowed share of the first pane.
    pub min_ratio: f32,
    /// Largest allowed share of the first pane.
    pub max_ratio: f32,
}

impl Default for Split {
    fn default() -> Self {
        Self {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            min_ratio: 0.1,
            max_ratio: 0.9,
        }
    }
}

impl Split {
    /// `ratio` clamped to the allowed range (a non-finite value keeps the current ratio).
    pub fn clamp_ratio(&self, ratio: f32) -> f32 {
        if !ratio.is_finite() {
            return self.ratio;
        }
        // A non-finite bound counts as the widest bound: NaN bounds let any ratio through.
        // Regression: tests/tqs_sweep.rs::split_clamp_ratio_stays_in_range_for_non_finite_bounds
        let finite = |v: f32, wide: f32| if v.is_finite() { v } else { wide };
        let (min_ratio, max_ratio) = (finite(self.min_ratio, 0.01), finite(self.max_ratio, 0.99));
        let lo = min_ratio.min(max_ratio).clamp(0.01, 0.99);
        let hi = min_ratio.max(max_ratio).clamp(0.01, 0.99);
        ratio.max(lo).min(hi)
    }

    /// The ratio after dragging the divider by `delta_px` over `total_px` of shared space.
    pub fn ratio_after_drag(&self, delta_px: f32, total_px: f32) -> f32 {
        if total_px < 1.0 || !delta_px.is_finite() {
            return self.ratio;
        }
        self.clamp_ratio(self.ratio + delta_px / total_px)
    }
}

/// Marks the draggable divider child of a [`Split`].
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SplitDivider;

/// Sets a split's ratio (clamped). Returns the stored ratio when it changed.
pub(crate) fn set_split_ratio(world: &mut World, entity: Entity, ratio: f32) -> Option<f32> {
    let mut split = world.get_mut::<Split>(entity)?;
    let next = split.clamp_ratio(ratio);
    if (next - split.ratio).abs() <= f32::EPSILON {
        return None;
    }
    split.ratio = next;
    world.write_message(UiEvent::new(
        entity,
        UiEventKind::ValueChanged(UiValue::Number(next)),
    ));
    Some(next)
}

/// Applies a divider drag of `delta` physical px to its split.
fn drag_split(world: &mut World, divider: Entity, delta: [f32; 2]) {
    let Some(split_entity) = world.get::<ChildOf>(divider).map(|c| c.0) else {
        return;
    };
    let Some(split) = world.get::<Split>(split_entity).copied() else {
        return;
    };
    let (Some(area), Some(bar)) = (
        world.get::<ComputedNode>(split_entity).copied(),
        world.get::<ComputedNode>(divider).copied(),
    ) else {
        return;
    };
    let (delta_px, total_px) = match split.axis {
        SplitAxis::Horizontal => (delta[0], area.rect[2] - bar.rect[2]),
        SplitAxis::Vertical => (delta[1], area.rect[3] - bar.rect[3]),
    };
    let next = split.ratio_after_drag(delta_px, total_px);
    set_split_ratio(world, split_entity, next);
}

/// A tab container (`.ui.ron` `tabs`, ADR-7827): its header child carries the
/// [`crate::widgets::TabBar`] (the active index lives there), and every child carrying [`TabPage`]
/// shows only while its index is the bar's active tab.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TabPages;

/// Marks the page of tab `index` under a [`TabPages`] container.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabPage {
    /// The tab this page belongs to.
    pub index: usize,
    /// The `display` value while shown (what the authored node had).
    pub display: Display,
}

/// The header child of a [`TabPages`] container (the one with the tab bar).
pub(crate) fn tab_pages_bar(world: &World, entity: Entity) -> Option<Entity> {
    world.get::<TabPages>(entity)?;
    world
        .get::<Children>(entity)?
        .iter()
        .find(|c| world.get::<crate::widgets::TabBar>(*c).is_some())
}

/// Selects tab `index` of a [`TabPages`] container (through its bar). Returns whether anything
/// changed (an out-of-range index or the active tab changes nothing).
pub(crate) fn select_tab_page(world: &mut World, entity: Entity, index: usize) -> bool {
    let Some(bar) = tab_pages_bar(world, entity) else {
        return false;
    };
    let Some(mut tabs) = world.get_mut::<crate::widgets::TabBar>(bar) else {
        return false;
    };
    if index >= tabs.tabs.len() || tabs.active == index {
        return false;
    }
    tabs.active = index;
    world.write_message(UiEvent::new(
        bar,
        UiEventKind::ValueChanged(UiValue::Index(index)),
    ));
    true
}

fn sync_tab_pages(world: &mut World) {
    let containers: Vec<Entity> = world
        .query_filtered::<Entity, With<TabPages>>()
        .iter(world)
        .collect();
    let mut dirty = false;
    for entity in containers {
        let active = tab_pages_bar(world, entity)
            .and_then(|bar| world.get::<crate::widgets::TabBar>(bar))
            .map(|bar| bar.active);
        let children: Vec<Entity> = world
            .get::<Children>(entity)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        for child in children {
            let Some(page) = world.get::<TabPage>(child).copied() else {
                continue;
            };
            let want = if Some(page.index) == active {
                page.display
            } else {
                Display::None
            };
            if let Some(mut style) = world.get_mut::<NodeStyle>(child)
                && style.0.display != want
            {
                style.0.display = want;
                dirty = true;
            }
        }
    }
    if dirty {
        world.resource_mut::<LayoutDirty>().mark();
    }
}

/// A path navigator (ADR-273 W11): `segments` runs from the root down to the current page, and
/// `current` is the shown page. Clicking a crumb (or `Submit` while it is focused) navigates to
/// that segment. Emits `ValueChanged(Index)` with the new position. An out-of-range `current`
/// (including any value while `segments` is empty) shows no crumb as current; only
/// `navigate_breadcrumbs` changes `current`, and it rejects out-of-range indices.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct Breadcrumbs {
    /// Path segments, root first.
    pub segments: Vec<String>,
    /// Shown segment (an index into `segments`).
    pub current: usize,
}

/// Marks a crumb button child of a [`Breadcrumbs`] with its segment index.
///
/// Managed by the `default_widgets` system; a crumb carries `Interactable`, `Focusable` and one
/// `Text` label child.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CrumbIndex(pub usize);

/// The separator drawn between two crumbs.
const CRUMB_SEPARATOR: &str = "›";

/// Navigates `entity`'s breadcrumbs to `index`. Returns whether anything changed (an unknown
/// entity, a disabled container, an out-of-range index or the current page changes nothing).
pub(crate) fn navigate_breadcrumbs(world: &mut World, entity: Entity, index: usize) -> bool {
    let Some(crumbs) = world.get::<Breadcrumbs>(entity).cloned() else {
        return false;
    };
    if crate::node::is_disabled(world, entity) {
        return false;
    }
    if index >= crumbs.segments.len() || index == crumbs.current {
        return false;
    }
    if let Some(mut crumbs) = world.get_mut::<Breadcrumbs>(entity) {
        crumbs.current = index;
    }
    world.write_message(UiEvent::new(
        entity,
        UiEventKind::ValueChanged(UiValue::Index(index)),
    ));
    true
}

/// Ensures each [`Breadcrumbs`] has one crumb button child per segment (in order, with a `›`
/// separator label between neighbours) and refreshes the labels and the `primary` class of the
/// current crumb. Children it manages are the ones carrying [`CrumbIndex`] and the plain `Text`
/// separator nodes with the `crumb-separator` class; anything else under the container is left
/// alone.
fn sync_breadcrumb_children(world: &mut World) {
    let bars: Vec<(Entity, Breadcrumbs)> = world
        .query::<(Entity, &Breadcrumbs)>()
        .iter(world)
        .map(|(e, b)| (e, b.clone()))
        .collect();
    for (entity, crumbs) in bars {
        let children: Vec<Entity> = world
            .get::<Children>(entity)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        let mut managed: Vec<(Entity, usize)> = children
            .iter()
            .filter_map(|c| world.get::<CrumbIndex>(*c).map(|i| (*c, i.0)))
            .collect();
        managed.sort_by_key(|(_, i)| *i);
        let fresh =
            managed.len() == crumbs.segments.len() && managed.iter().enumerate().all(|(at, (_, i))| at == *i);
        if fresh {
            let mut dirty = false;
            for (child, index) in managed {
                let label = crumbs.segments.get(index).cloned().unwrap_or_default();
                if refresh_crumb_label(world, child, &label) {
                    dirty = true;
                }
                let mut classes = world.get::<Classes>(child).cloned().unwrap_or_default();
                let want_primary = index == crumbs.current;
                let has_primary = classes.0.iter().any(|c| c == "primary");
                if want_primary != has_primary {
                    classes.0.retain(|c| c != "primary");
                    if want_primary {
                        classes.0.push("primary".into());
                    }
                    world.entity_mut(child).insert(classes);
                    dirty = true;
                }
            }
            if dirty {
                world.resource_mut::<LayoutDirty>().mark();
            }
            continue;
        }
        for (child, _) in managed {
            world.entity_mut(child).despawn();
        }
        for child in children {
            if world.get::<CrumbIndex>(child).is_some() {
                continue;
            }
            let is_separator = world.get::<Text>(child).is_some()
                && world
                    .get::<Classes>(child)
                    .is_some_and(|c| c.0.iter().any(|c| c == "crumb-separator"));
            if is_separator {
                world.entity_mut(child).despawn();
            }
        }
        for (index, label) in crumbs.segments.iter().enumerate() {
            let mut classes = vec!["button".to_owned(), "crumb".to_owned()];
            if index == crumbs.current {
                classes.push("primary".into());
            }
            let child = world
                .spawn((
                    UiNode,
                    NodeStyle::new(Style {
                        padding: Edges::px(8.0),
                        justify_content: JustifyContent::Center,
                        align_items: Align::Center,
                        min_size: [Dimension::Auto, Dimension::Points(32.0)],
                        ..Default::default()
                    }),
                    Interactable::default(),
                    crate::focus::Focusable::default(),
                    CrumbIndex(index),
                    Classes(classes),
                    Themed::default(),
                ))
                .with_children(|crumb| {
                    crumb.spawn((
                        UiNode,
                        Text {
                            spans: vec![TextSpan::lit(label.clone())],
                            wrap: WrapMode::None,
                            ..Default::default()
                        },
                    ));
                })
                .id();
            world.entity_mut(entity).add_child(child);
            if index + 1 < crumbs.segments.len() {
                let separator = world
                    .spawn((
                        UiNode,
                        Text {
                            spans: vec![TextSpan::lit(CRUMB_SEPARATOR)],
                            wrap: WrapMode::None,
                            ..Default::default()
                        },
                        Classes(vec!["crumb-separator".to_owned()]),
                    ))
                    .id();
                world.entity_mut(entity).add_child(separator);
            }
        }
        world.resource_mut::<LayoutDirty>().mark();
    }
}

/// Sets a crumb button's label text, creating the `Text` child when missing. Returns whether the
/// tree changed.
fn refresh_crumb_label(world: &mut World, crumb: Entity, label: &str) -> bool {
    let text_child = world
        .get::<Children>(crumb)
        .and_then(|c| c.iter().find(|e| world.get::<Text>(*e).is_some()));
    match text_child {
        Some(child) => {
            if let Some(mut text) = world.get_mut::<Text>(child) {
                let current = text.spans.first().and_then(|s| match &s.content {
                    TextContent::Lit(t) => Some(t.as_str()),
                    _ => None,
                });
                if current != Some(label) {
                    text.spans = vec![TextSpan::lit(label.to_owned())];
                    return true;
                }
            }
            false
        }
        None => {
            let child = world
                .spawn((
                    UiNode,
                    Text {
                        spans: vec![TextSpan::lit(label.to_owned())],
                        wrap: WrapMode::None,
                        ..Default::default()
                    },
                ))
                .id();
            world.entity_mut(crumb).add_child(child);
            true
        }
    }
}

/// Where a toast stack lives: toasts are spawned as its children (newest last). Place the node
/// where the stack should appear (typically absolute, in a screen corner).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToastHost {
    /// Most toasts shown at once; further requests wait in the [`ToastQueue`].
    pub max_visible: usize,
}

impl Default for ToastHost {
    fn default() -> Self {
        Self { max_visible: 4 }
    }
}

/// A shown notification. Counts down in game time and is despawned at zero, or when clicked.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Toast {
    /// Seconds left (`f32::INFINITY` = until clicked).
    pub remaining: f32,
}

/// Notifications waiting for room in a [`ToastHost`].
#[derive(Resource, Debug, Default, Clone, PartialEq)]
pub struct ToastQueue {
    pending: VecDeque<(String, f32)>,
}

impl ToastQueue {
    /// Queues a toast for `seconds` (zero or negative = until clicked). Past
    /// 64 waiting requests the oldest is dropped.
    pub fn push(&mut self, text: impl Into<String>, seconds: f32) {
        let seconds = if seconds > 0.0 && seconds.is_finite() {
            seconds
        } else {
            f32::INFINITY
        };
        if self.pending.len() >= MAX_QUEUED_TOASTS {
            self.pending.pop_front();
        }
        self.pending.push_back((text.into(), seconds));
    }

    /// Requests still waiting.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether nothing is waiting.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Spawns queued toasts into the first host (lowest entity id) that has room, then ages the shown
/// ones by `dt` seconds and despawns the expired. Returns how many toasts expired.
pub(crate) fn tick_toasts(world: &mut World, dt: f32) -> usize {
    let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
    let mut expired = Vec::new();
    for (entity, mut toast) in world.query::<(Entity, &mut Toast)>().iter_mut(world) {
        toast.remaining -= dt;
        if toast.remaining <= 0.0 {
            expired.push(entity);
        }
    }
    let count = expired.len();
    for entity in expired {
        world.entity_mut(entity).despawn();
    }
    if count > 0 {
        world.resource_mut::<LayoutDirty>().mark();
    }
    fill_toast_host(world);
    count
}

fn fill_toast_host(world: &mut World) {
    if world
        .get_resource::<ToastQueue>()
        .is_none_or(ToastQueue::is_empty)
    {
        return;
    }
    let mut hosts: Vec<(Entity, ToastHost)> = world
        .query::<(Entity, &ToastHost)>()
        .iter(world)
        .map(|(e, h)| (e, *h))
        .collect();
    hosts.sort_by_key(|(e, _)| e.to_bits());
    let Some((host, config)) = hosts.into_iter().next() else {
        return;
    };
    let shown = world
        .get::<Children>(host)
        .map(|c| c.iter().filter(|e| world.get::<Toast>(*e).is_some()).count())
        .unwrap_or(0);
    let room = config.max_visible.saturating_sub(shown);
    for _ in 0..room {
        let Some((text, seconds)) = world.resource_mut::<ToastQueue>().pending.pop_front() else {
            break;
        };
        let toast = world
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    padding: Edges::px(12.0),
                    ..Default::default()
                }),
                Interactable::default(),
                Toast { remaining: seconds },
                Classes(vec!["surface".to_owned(), "toast".to_owned()]),
                Themed::default(),
            ))
            .with_children(|t| {
                t.spawn((
                    UiNode,
                    Text {
                        spans: vec![TextSpan::lit(text)],
                        wrap: WrapMode::None,
                        ..Default::default()
                    },
                ));
            })
            .id();
        world.entity_mut(host).add_child(toast);
        world.resource_mut::<LayoutDirty>().mark();
    }
}

/// Sets the first `Text` child of `entity` to `label`, creating the child when missing.
fn set_label(world: &mut World, entity: Entity, label: &str) {
    let children: Vec<Entity> = world
        .get::<Children>(entity)
        .map(|c| c.iter().collect())
        .unwrap_or_default();
    let text_child = children.into_iter().find(|c| world.get::<Text>(*c).is_some());
    match text_child {
        Some(child) => {
            if let Some(mut text) = world.get_mut::<Text>(child) {
                let current = text.spans.first().and_then(|s| match &s.content {
                    TextContent::Lit(t) => Some(t.as_str()),
                    _ => None,
                });
                if current != Some(label) {
                    text.spans = vec![TextSpan::lit(label.to_owned())];
                }
            }
        }
        None => {
            let child = world
                .spawn((
                    UiNode,
                    Text {
                        spans: vec![TextSpan::lit(label.to_owned())],
                        wrap: WrapMode::None,
                        ..Default::default()
                    },
                ))
                .id();
            world.entity_mut(entity).add_child(child);
            world.resource_mut::<LayoutDirty>().mark();
        }
    }
}

/// Applies an agent `ui.set_value` to one of the D11 widgets or the W11 breadcrumbs.
/// `None` = `entity` is not one of them or the value type does not fit; `Some(false)` = nothing
/// changed.
pub(crate) fn apply_widget_value(world: &mut World, entity: Entity, value: &UiValue) -> Option<bool> {
    match value {
        UiValue::Bool(on) => {
            if world.get::<RadioButton>(entity).is_some() {
                // A radio cannot be unchecked directly; check another one of its group.
                Some(*on && select_radio(world, entity))
            } else if world.get::<Foldable>(entity).is_some() {
                Some(set_fold(world, entity, *on))
            } else {
                None
            }
        }
        UiValue::Number(n) => {
            if world.get::<SpinBox>(entity).is_some() {
                Some(set_spin_box(world, entity, *n).is_some())
            } else if world.get::<Split>(entity).is_some() {
                Some(set_split_ratio(world, entity, *n).is_some())
            } else {
                None
            }
        }
        UiValue::Index(i) => {
            if world.get::<Breadcrumbs>(entity).is_some() {
                Some(navigate_breadcrumbs(world, entity, *i))
            } else if world.get::<TabPages>(entity).is_some() {
                Some(select_tab_page(world, entity, *i))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The value `ui.query` reports for one of the D11 widgets (or the W11 breadcrumbs).
pub(crate) fn widget_value(world: &World, entity: Entity) -> Option<serde_json::Value> {
    if let Some(radio) = world.get::<RadioButton>(entity) {
        Some(serde_json::json!(radio.checked))
    } else if let Some(spin) = world.get::<SpinBox>(entity) {
        Some(serde_json::json!(spin.value))
    } else if let Some(fold) = world.get::<Foldable>(entity) {
        Some(serde_json::json!(fold.open))
    } else if let Some(crumbs) = world.get::<Breadcrumbs>(entity) {
        Some(serde_json::json!(crumbs.current))
    } else if world.get::<TabPages>(entity).is_some() {
        tab_pages_bar(world, entity)
            .and_then(|bar| world.get::<crate::widgets::TabBar>(bar))
            .map(|bar| serde_json::json!(bar.active))
    } else {
        world.get::<Split>(entity).map(|s| serde_json::json!(s.ratio))
    }
}

/// The type name `ui.query` reports for one of the D11 widgets (or the W11 breadcrumbs).
pub(crate) fn widget_kind(world: &World, entity: Entity) -> Option<&'static str> {
    if world.get::<RadioButton>(entity).is_some() {
        Some("RadioButton")
    } else if world.get::<SpinBox>(entity).is_some() {
        Some("SpinBox")
    } else if world.get::<Foldable>(entity).is_some() {
        Some("Foldable")
    } else if world.get::<Breadcrumbs>(entity).is_some() {
        Some("Breadcrumbs")
    } else if world.get::<Split>(entity).is_some() {
        Some("Split")
    } else if world.get::<Toast>(entity).is_some() {
        Some("Toast")
    } else if world.get::<TabPages>(entity).is_some() {
        Some("Tabs")
    } else {
        None
    }
}

/// `Update` (called from [`crate::widgets::ui_widgets`]): reacts to this frame's events and keeps
/// the looks in sync with the component state.
pub(crate) fn default_widgets(world: &mut World) {
    let _span = tracing::trace_span!("ui_default_widgets").entered();
    let events: Vec<UiEvent> = world
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .cloned()
        .collect();
    for event in &events {
        react(world, event);
    }
    sync_radio_looks(world);
    sync_spin_labels(world);
    sync_folds(world);
    sync_splits(world);
    sync_tab_pages(world);
    sync_breadcrumb_children(world);
    let dt = world.get_resource::<Time>().map_or(0.0, |t| t.delta_secs());
    tick_toasts(world, dt);
}

fn react(world: &mut World, event: &UiEvent) {
    let target = event.target;
    match &event.kind {
        UiEventKind::Click => {
            if world.get::<RadioButton>(target).is_some() {
                select_radio(world, target);
            } else if world.get::<FoldHeader>(target).is_some() {
                if let Some(fold) = world.get::<ChildOf>(target).map(|c| c.0)
                    && let Some(open) = world.get::<Foldable>(fold).map(|f| f.open)
                {
                    set_fold(world, fold, !open);
                }
            } else if world.get::<Foldable>(target).is_some() {
                let open = world.get::<Foldable>(target).is_some_and(|f| f.open);
                set_fold(world, target, !open);
            } else if world.get::<Toast>(target).is_some() {
                world.entity_mut(target).despawn();
                world.resource_mut::<LayoutDirty>().mark();
            } else if world.get::<SpinBox>(target).is_some() {
                click_spin_box(world, target);
            } else if let Some(index) = world.get::<CrumbIndex>(target).map(|c| c.0)
                && let Some(parent) = world.get::<ChildOf>(target).map(|c| c.0)
            {
                navigate_breadcrumbs(world, parent, index);
            }
        }
        UiEventKind::Drag(delta) => {
            if world.get::<SplitDivider>(target).is_some() {
                drag_split(world, target, *delta);
            } else if world.get::<SpinBox>(target).is_some() {
                let steps = {
                    let Some(mut spin) = world.get_mut::<SpinBox>(target) else {
                        return;
                    };
                    spin.drag_accum += delta[0];
                    let px = spin.px_per_step.max(1.0);
                    let steps = (spin.drag_accum / px).trunc();
                    spin.drag_accum -= steps * px;
                    steps as i32
                };
                if steps != 0 {
                    step_spin_box(world, target, steps);
                }
            }
        }
        UiEventKind::DragEnd => {
            if let Some(mut spin) = world.get_mut::<SpinBox>(target) {
                spin.drag_accum = 0.0;
            }
        }
        UiEventKind::Scroll(delta) if world.get::<SpinBox>(target).is_some() && delta[1] != 0.0 => {
            step_spin_box(world, target, if delta[1] > 0.0 { 1 } else { -1 });
        }
        _ => {}
    }
}

/// A click on a spin box's left or right edge steps down or up; the middle does nothing.
fn click_spin_box(world: &mut World, entity: Entity) {
    let Some(computed) = world.get::<ComputedNode>(entity).copied() else {
        return;
    };
    let Some(p) = world.get_resource::<UiPointer>().and_then(UiPointer::position) else {
        return;
    };
    let edge = (computed.rect[2] * 0.25).min(24.0);
    let x = p[0] - computed.rect[0];
    if x <= edge {
        step_spin_box(world, entity, -1);
    } else if x >= computed.rect[2] - edge {
        step_spin_box(world, entity, 1);
    }
}

fn sync_radio_looks(world: &mut World) {
    let radios: Vec<(Entity, bool)> = world
        .query::<(Entity, &RadioButton)>()
        .iter(world)
        .map(|(e, r)| (e, r.checked))
        .collect();
    for (entity, checked) in radios {
        let has = world.get::<Selected>(entity).is_some();
        if checked && !has {
            world.entity_mut(entity).insert(Selected);
        } else if !checked && has {
            world.entity_mut(entity).remove::<Selected>();
        }
    }
}

fn sync_spin_labels(world: &mut World) {
    let spins: Vec<(Entity, String)> = world
        .query::<(Entity, &SpinBox)>()
        .iter(world)
        .map(|(e, s)| (e, s.display()))
        .collect();
    for (entity, label) in spins {
        set_label(world, entity, &label);
    }
}

fn sync_folds(world: &mut World) {
    let folds: Vec<(Entity, bool)> = world
        .query::<(Entity, &Foldable)>()
        .iter(world)
        .map(|(e, f)| (e, f.open))
        .collect();
    let mut dirty = false;
    for (entity, open) in folds {
        let children: Vec<Entity> = world
            .get::<Children>(entity)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        for child in children {
            if world.get::<FoldHeader>(child).is_some() {
                let has = world.get::<Selected>(child).is_some();
                if open && !has {
                    world.entity_mut(child).insert(Selected);
                } else if !open && has {
                    world.entity_mut(child).remove::<Selected>();
                }
            }
            let Some(body) = world.get::<FoldBody>(child).copied() else {
                continue;
            };
            let want = if open { body.display } else { Display::None };
            if let Some(mut style) = world.get_mut::<NodeStyle>(child)
                && style.0.display != want
            {
                style.0.display = want;
                dirty = true;
            }
        }
    }
    if dirty {
        world.resource_mut::<LayoutDirty>().mark();
    }
}

fn sync_splits(world: &mut World) {
    let splits: Vec<(Entity, Split)> = world
        .query::<(Entity, &Split)>()
        .iter(world)
        .map(|(e, s)| (e, *s))
        .collect();
    let mut dirty = false;
    for (entity, split) in splits {
        let children: Vec<Entity> = world
            .get::<Children>(entity)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        let Some(at) = children
            .iter()
            .position(|c| world.get::<SplitDivider>(*c).is_some())
        else {
            continue;
        };
        let first = at.checked_sub(1).and_then(|i| children.get(i)).copied();
        let second = children.get(at + 1).copied();
        let ratio = split.clamp_ratio(split.ratio);
        for (pane, grow) in [(first, ratio), (second, 1.0 - ratio)] {
            let Some(pane) = pane else {
                continue;
            };
            if let Some(mut style) = world.get_mut::<NodeStyle>(pane)
                && ((style.0.flex_grow - grow).abs() > 1e-6 || style.0.flex_basis != Dimension::Points(0.0))
            {
                style.0.flex_grow = grow;
                style.0.flex_basis = Dimension::Points(0.0);
                dirty = true;
            }
        }
    }
    if dirty {
        world.resource_mut::<LayoutDirty>().mark();
    }
}

/// Spawns a radio button with a label under `parent`. Returns the button entity.
pub fn spawn_radio_button(
    commands: &mut Commands,
    parent: Entity,
    label: &str,
    group: &str,
    checked: bool,
) -> Entity {
    let entity = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                padding: Edges::px(8.0),
                align_items: Align::Center,
                ..Default::default()
            }),
            Interactable::default(),
            crate::focus::Focusable::default(),
            RadioButton {
                group: group.to_owned(),
                checked,
            },
            Classes(vec!["button".to_owned(), "radio".to_owned()]),
            Themed::default(),
        ))
        .with_children(|b| {
            b.spawn((
                UiNode,
                Text {
                    spans: vec![TextSpan::lit(label.to_owned())],
                    wrap: WrapMode::None,
                    ..Default::default()
                },
            ));
        })
        .id();
    commands.entity(parent).add_child(entity);
    entity
}

/// Spawns a foldable with a titled header under `parent`. Returns `(foldable, body)`; add the
/// content as children of `body`.
pub fn spawn_foldable(
    commands: &mut Commands,
    parent: Entity,
    title: &str,
    group: Option<&str>,
    open: bool,
) -> (Entity, Entity) {
    let body = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                padding: Edges::px(8.0),
                display: if open { Display::Flex } else { Display::None },
                ..Default::default()
            }),
            FoldBody::default(),
        ))
        .id();
    let header = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                padding: Edges::px(8.0),
                align_items: Align::Center,
                ..Default::default()
            }),
            Interactable::default(),
            crate::focus::Focusable::default(),
            FoldHeader,
            Classes(vec!["button".to_owned(), "fold-header".to_owned()]),
            Themed::default(),
        ))
        .with_children(|h| {
            h.spawn((
                UiNode,
                Text {
                    spans: vec![TextSpan::lit(title.to_owned())],
                    wrap: WrapMode::None,
                    ..Default::default()
                },
            ));
        })
        .id();
    let fold = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                ..Default::default()
            }),
            Foldable {
                open,
                group: group.map(str::to_owned),
            },
        ))
        .id();
    commands.entity(fold).add_children(&[header, body]);
    commands.entity(parent).add_child(fold);
    (fold, body)
}

/// Spawns a split with a divider under `parent`, empty panes either side. Returns
/// `(split, first_pane, second_pane)`.
pub fn spawn_split(
    commands: &mut Commands,
    parent: Entity,
    axis: SplitAxis,
    ratio: f32,
) -> (Entity, Entity, Entity) {
    let pane = |commands: &mut Commands| {
        commands
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    flex_grow: 0.5,
                    flex_basis: Dimension::Points(0.0),
                    ..Default::default()
                }),
            ))
            .id()
    };
    let first = pane(commands);
    let second = pane(commands);
    let thickness = 6.0;
    let divider = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: match axis {
                    SplitAxis::Horizontal => [Dimension::Points(thickness), Dimension::Auto],
                    SplitAxis::Vertical => [Dimension::Auto, Dimension::Points(thickness)],
                },
                justify_content: JustifyContent::Center,
                ..Default::default()
            }),
            Interactable::default(),
            SplitDivider,
            Classes::one("split-divider"),
            Themed::default(),
        ))
        .id();
    let split = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: match axis {
                    SplitAxis::Horizontal => FlexDirection::Row,
                    SplitAxis::Vertical => FlexDirection::Column,
                },
                ..Default::default()
            }),
            Split {
                axis,
                ratio,
                ..Default::default()
            },
        ))
        .id();
    commands.entity(split).add_children(&[first, divider, second]);
    commands.entity(parent).add_child(split);
    (split, first, second)
}

/// Spawns breadcrumbs under `parent` with one crumb per segment. Returns the container entity.
/// The crumb buttons (and the `›` separators between them) are built by `default_widgets` on the
/// next frame, so the container starts childless; `current` is the shown segment and should be an
/// index into `segments` (an out-of-range value shows no crumb as current until navigated).
pub fn spawn_breadcrumbs(
    commands: &mut Commands,
    parent: Entity,
    segments: &[&str],
    current: usize,
) -> Entity {
    let entity = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                align_items: Align::Center,
                gap: [Dimension::Points(4.0), Dimension::Points(4.0)],
                ..Default::default()
            }),
            Breadcrumbs {
                segments: segments.iter().map(|s| (*s).to_owned()).collect(),
                current,
            },
            Classes(vec!["breadcrumbs".to_owned()]),
            Themed::default(),
        ))
        .id();
    commands.entity(parent).add_child(entity);
    entity
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<Messages<UiEvent>>();
        world.init_resource::<LayoutDirty>();
        world.init_resource::<ToastQueue>();
        world
    }

    fn events(world: &mut World) -> Vec<UiEvent> {
        world
            .resource_mut::<Messages<UiEvent>>()
            .iter_current_update_messages()
            .cloned()
            .collect()
    }

    fn radio(world: &mut World, parent: Entity, group: &str, checked: bool) -> Entity {
        let e = world
            .spawn(RadioButton {
                group: group.to_owned(),
                checked,
            })
            .id();
        world.entity_mut(parent).add_child(e);
        e
    }

    #[test]
    fn radio_selection_is_exclusive_within_group_and_parent() {
        let mut w = world();
        let (p1, p2) = (w.spawn_empty().id(), w.spawn_empty().id());
        let a = radio(&mut w, p1, "mode", true);
        let b = radio(&mut w, p1, "mode", false);
        let other_group = radio(&mut w, p1, "size", true);
        let other_parent = radio(&mut w, p2, "mode", true);
        assert!(select_radio(&mut w, b));
        assert!(!w.get::<RadioButton>(a).unwrap().checked);
        assert!(w.get::<RadioButton>(b).unwrap().checked);
        assert!(w.get::<RadioButton>(other_group).unwrap().checked);
        assert!(w.get::<RadioButton>(other_parent).unwrap().checked);
        assert!(w.get::<Selected>(b).is_some());
        assert!(w.get::<Selected>(a).is_none());
        let ev = events(&mut w);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].kind, UiEventKind::ValueChanged(UiValue::Bool(false)));
        assert_eq!(ev[1].kind, UiEventKind::ValueChanged(UiValue::Bool(true)));
    }

    #[test]
    fn radio_reselect_and_disabled_change_nothing() {
        let mut w = world();
        let p = w.spawn_empty().id();
        let a = radio(&mut w, p, "g", true);
        let b = radio(&mut w, p, "g", false);
        assert!(!select_radio(&mut w, a));
        w.entity_mut(b).insert(crate::node::Disabled);
        assert!(!select_radio(&mut w, b));
        assert!(w.get::<RadioButton>(a).unwrap().checked);
        assert!(events(&mut w).is_empty());
    }

    #[test]
    fn spin_box_snaps_clamps_and_formats() {
        let spin = SpinBox {
            value: 1.0,
            min: 0.0,
            max: 2.0,
            step: 0.25,
            decimals: 2,
            prefix: "x".into(),
            suffix: " m".into(),
            ..Default::default()
        };
        assert_eq!(spin.snap(0.3), 0.25);
        assert_eq!(spin.snap(9.0), 2.0);
        assert_eq!(spin.snap(-9.0), 0.0);
        assert_eq!(spin.snap(f32::NAN), 1.0);
        assert_eq!(spin.stepped(2), 1.5);
        assert_eq!(spin.stepped(-100), 0.0);
        assert_eq!(spin.display(), "x1.00 m");
    }

    #[test]
    fn spin_box_inverted_range_does_not_panic() {
        let spin = SpinBox {
            min: 5.0,
            max: 1.0,
            step: 0.0,
            ..Default::default()
        };
        assert_eq!(spin.snap(100.0), 5.0);
        assert_eq!(spin.snap(-100.0), 1.0);
    }

    #[test]
    fn spin_box_step_emits_only_on_change() {
        let mut w = world();
        let e = w
            .spawn(SpinBox {
                value: 9.0,
                max: 10.0,
                ..Default::default()
            })
            .id();
        assert_eq!(step_spin_box(&mut w, e, 1), Some(10.0));
        assert_eq!(step_spin_box(&mut w, e, 1), None);
        assert_eq!(set_spin_box(&mut w, e, 3.4), Some(3.0));
        let ev = events(&mut w);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[1].kind, UiEventKind::ValueChanged(UiValue::Number(3.0)));
        w.entity_mut(e).insert(crate::node::Disabled);
        assert_eq!(step_spin_box(&mut w, e, 1), None);
    }

    #[test]
    fn spin_box_drag_accumulates_whole_steps() {
        let mut w = world();
        let e = w
            .spawn(SpinBox {
                px_per_step: 10.0,
                ..Default::default()
            })
            .id();
        react(&mut w, &UiEvent::new(e, UiEventKind::Drag([6.0, 0.0])));
        assert_eq!(w.get::<SpinBox>(e).unwrap().value, 0.0);
        react(&mut w, &UiEvent::new(e, UiEventKind::Drag([6.0, 0.0])));
        assert_eq!(w.get::<SpinBox>(e).unwrap().value, 1.0);
        react(&mut w, &UiEvent::new(e, UiEventKind::Drag([-35.0, 0.0])));
        assert_eq!(w.get::<SpinBox>(e).unwrap().value, 0.0);
        react(&mut w, &UiEvent::new(e, UiEventKind::DragEnd));
        assert_eq!(w.get::<SpinBox>(e).unwrap().drag_accum, 0.0);
    }

    #[test]
    fn spin_box_wheel_steps_by_sign() {
        let mut w = world();
        let e = w.spawn(SpinBox::default()).id();
        react(&mut w, &UiEvent::new(e, UiEventKind::Scroll([0.0, 40.0])));
        react(&mut w, &UiEvent::new(e, UiEventKind::Scroll([0.0, 40.0])));
        react(&mut w, &UiEvent::new(e, UiEventKind::Scroll([0.0, -5.0])));
        assert_eq!(w.get::<SpinBox>(e).unwrap().value, 1.0);
    }

    #[test]
    fn foldable_group_is_an_accordion() {
        let mut w = world();
        let p = w.spawn_empty().id();
        let mk = |w: &mut World, group: Option<&str>, open: bool| {
            let e = w
                .spawn(Foldable {
                    open,
                    group: group.map(str::to_owned),
                })
                .id();
            w.entity_mut(p).add_child(e);
            e
        };
        let a = mk(&mut w, Some("g"), true);
        let b = mk(&mut w, Some("g"), false);
        let free = mk(&mut w, None, true);
        assert!(set_fold(&mut w, b, true));
        assert!(!w.get::<Foldable>(a).unwrap().open);
        assert!(w.get::<Foldable>(b).unwrap().open);
        assert!(w.get::<Foldable>(free).unwrap().open);
        assert!(!set_fold(&mut w, b, true));
        assert!(set_fold(&mut w, b, false));
        assert!(!w.get::<Foldable>(b).unwrap().open);
    }

    #[test]
    fn foldable_body_display_follows_open_state() {
        let mut w = world();
        let fold = w
            .spawn(Foldable {
                open: false,
                group: None,
            })
            .id();
        let header = w.spawn((FoldHeader, NodeStyle::default())).id();
        let body = w
            .spawn((
                FoldBody {
                    display: Display::Grid,
                },
                NodeStyle::default(),
            ))
            .id();
        w.entity_mut(fold).add_children(&[header, body]);
        sync_folds(&mut w);
        assert_eq!(w.get::<NodeStyle>(body).unwrap().0.display, Display::None);
        assert!(w.get::<Selected>(header).is_none());
        react(&mut w, &UiEvent::new(header, UiEventKind::Click));
        sync_folds(&mut w);
        assert_eq!(w.get::<NodeStyle>(body).unwrap().0.display, Display::Grid);
        assert!(w.get::<Selected>(header).is_some());
    }

    #[test]
    fn split_ratio_math_clamps() {
        let split = Split::default();
        assert!((split.ratio_after_drag(50.0, 200.0) - 0.75).abs() < 1e-6);
        assert_eq!(split.ratio_after_drag(1000.0, 200.0), 0.9);
        assert_eq!(split.ratio_after_drag(-1000.0, 200.0), 0.1);
        assert_eq!(split.ratio_after_drag(10.0, 0.0), 0.5);
        assert_eq!(split.ratio_after_drag(f32::NAN, 200.0), 0.5);
        assert_eq!(split.clamp_ratio(f32::NAN), 0.5);
    }

    #[test]
    fn split_divider_drag_moves_ratio_and_panes_follow() {
        let mut w = world();
        let split = w
            .spawn((
                Split::default(),
                ComputedNode {
                    rect: [0.0, 0.0, 206.0, 100.0],
                    ..Default::default()
                },
            ))
            .id();
        let first = w.spawn(NodeStyle::default()).id();
        let divider = w
            .spawn((
                SplitDivider,
                ComputedNode {
                    rect: [100.0, 0.0, 6.0, 100.0],
                    ..Default::default()
                },
            ))
            .id();
        let second = w.spawn(NodeStyle::default()).id();
        w.entity_mut(split).add_children(&[first, divider, second]);
        react(&mut w, &UiEvent::new(divider, UiEventKind::Drag([50.0, 9.0])));
        assert!((w.get::<Split>(split).unwrap().ratio - 0.75).abs() < 1e-6);
        sync_splits(&mut w);
        assert!((w.get::<NodeStyle>(first).unwrap().0.flex_grow - 0.75).abs() < 1e-6);
        assert!((w.get::<NodeStyle>(second).unwrap().0.flex_grow - 0.25).abs() < 1e-6);
        let ev = events(&mut w);
        assert_eq!(ev.len(), 1);
    }

    #[test]
    fn vertical_split_uses_the_y_delta() {
        let mut w = world();
        let split = w
            .spawn((
                Split {
                    axis: SplitAxis::Vertical,
                    ..Default::default()
                },
                ComputedNode {
                    rect: [0.0, 0.0, 100.0, 206.0],
                    ..Default::default()
                },
            ))
            .id();
        let divider = w
            .spawn((
                SplitDivider,
                ComputedNode {
                    rect: [0.0, 100.0, 100.0, 6.0],
                    ..Default::default()
                },
            ))
            .id();
        w.entity_mut(split).add_child(divider);
        react(&mut w, &UiEvent::new(divider, UiEventKind::Drag([99.0, -50.0])));
        assert!((w.get::<Split>(split).unwrap().ratio - 0.25).abs() < 1e-6);
    }

    #[test]
    fn toasts_wait_for_room_then_expire_on_a_fixed_step() {
        let mut w = world();
        let host = w.spawn(ToastHost { max_visible: 2 }).id();
        {
            let mut q = w.resource_mut::<ToastQueue>();
            q.push("one", 1.0);
            q.push("two", 3.0);
            q.push("three", 3.0);
        }
        assert_eq!(tick_toasts(&mut w, 0.0), 0);
        let shown = |w: &World| {
            w.get::<Children>(host)
                .map(|c| c.iter().filter(|e| w.get::<Toast>(*e).is_some()).count())
                .unwrap_or(0)
        };
        assert_eq!(shown(&w), 2);
        assert_eq!(w.resource::<ToastQueue>().len(), 1);
        // Step 1 s: "one" expires and "three" takes its place in the same tick.
        assert_eq!(tick_toasts(&mut w, 1.0), 1);
        assert_eq!(shown(&w), 2);
        assert!(w.resource::<ToastQueue>().is_empty());
        assert_eq!(tick_toasts(&mut w, 2.0), 1);
        assert_eq!(tick_toasts(&mut w, 1.0), 1);
        assert_eq!(shown(&w), 0);
    }

    #[test]
    fn sticky_toast_stays_until_clicked() {
        let mut w = world();
        let host = w.spawn(ToastHost::default()).id();
        w.resource_mut::<ToastQueue>().push("stuck", 0.0);
        tick_toasts(&mut w, 0.0);
        assert_eq!(tick_toasts(&mut w, 1.0e6), 0);
        let toast = w
            .get::<Children>(host)
            .and_then(|c| c.iter().find(|e| w.get::<Toast>(*e).is_some()))
            .unwrap();
        react(&mut w, &UiEvent::new(toast, UiEventKind::Click));
        assert!(w.get_entity(toast).is_err());
    }

    #[test]
    fn toast_queue_drops_the_oldest_past_the_cap() {
        let mut q = ToastQueue::default();
        for i in 0..70 {
            q.push(format!("t{i}"), 1.0);
        }
        assert_eq!(q.len(), MAX_QUEUED_TOASTS);
        assert_eq!(q.pending.front().map(|(t, _)| t.as_str()), Some("t6"));
    }

    #[test]
    fn agent_values_reach_the_widgets() {
        let mut w = world();
        let p = w.spawn_empty().id();
        let r = radio(&mut w, p, "g", false);
        let s = w.spawn(SpinBox::default()).id();
        assert_eq!(apply_widget_value(&mut w, r, &UiValue::Bool(true)), Some(true));
        assert_eq!(apply_widget_value(&mut w, r, &UiValue::Bool(false)), Some(false));
        assert_eq!(apply_widget_value(&mut w, s, &UiValue::Number(7.2)), Some(true));
        assert_eq!(apply_widget_value(&mut w, s, &UiValue::Bool(true)), None);
        assert_eq!(widget_value(&w, s), Some(serde_json::json!(7.0)));
        assert_eq!(widget_kind(&w, r), Some("RadioButton"));
    }

    fn crumbs(world: &mut World, segments: &[&str], current: usize) -> Entity {
        world
            .spawn(Breadcrumbs {
                segments: segments.iter().map(|s| (*s).to_owned()).collect(),
                current,
            })
            .id()
    }

    fn crumb_labels(world: &World, entity: Entity) -> Vec<String> {
        let kids: Vec<Entity> = world
            .get::<Children>(entity)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        let mut crumbs: Vec<(usize, String)> = kids
            .into_iter()
            .filter_map(|c| {
                let index = world.get::<CrumbIndex>(c)?.0;
                let text = world
                    .get::<Children>(c)
                    .and_then(|k| k.iter().find(|e| world.get::<Text>(*e).is_some()));
                let label = text
                    .and_then(|t| world.get::<Text>(t))
                    .and_then(|t| t.spans.first())
                    .and_then(|s| match &s.content {
                        TextContent::Lit(t) => Some(t.clone()),
                        _ => None,
                    })?;
                Some((index, label))
            })
            .collect();
        crumbs.sort_by_key(|(i, _)| *i);
        crumbs.into_iter().map(|(_, l)| l).collect()
    }

    fn separators(world: &World, entity: Entity) -> usize {
        let kids: Vec<Entity> = world
            .get::<Children>(entity)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        kids.iter()
            .filter(|e| {
                world.get::<CrumbIndex>(**e).is_none()
                    && world
                        .get::<Classes>(**e)
                        .is_some_and(|c| c.0.iter().any(|c| c == "crumb-separator"))
            })
            .count()
    }

    #[test]
    fn breadcrumbs_navigate_emits_index_only_on_change() {
        let mut w = world();
        let e = crumbs(&mut w, &["Root", "Settings", "Audio"], 2);
        assert!(navigate_breadcrumbs(&mut w, e, 0));
        assert_eq!(w.get::<Breadcrumbs>(e).unwrap().current, 0);
        assert!(!navigate_breadcrumbs(&mut w, e, 0));
        assert!(!navigate_breadcrumbs(&mut w, e, 9));
        w.entity_mut(e).insert(crate::node::Disabled);
        assert!(!navigate_breadcrumbs(&mut w, e, 1));
        assert_eq!(w.get::<Breadcrumbs>(e).unwrap().current, 0);
        let ev = events(&mut w);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].target, e);
        assert_eq!(ev[0].kind, UiEventKind::ValueChanged(UiValue::Index(0)));
    }

    #[test]
    fn breadcrumbs_click_on_a_crumb_navigates() {
        let mut w = world();
        let e = crumbs(&mut w, &["Root", "Settings"], 1);
        let crumb = w.spawn(CrumbIndex(0)).id();
        w.entity_mut(e).add_child(crumb);
        react(&mut w, &UiEvent::new(crumb, UiEventKind::Click));
        assert_eq!(w.get::<Breadcrumbs>(e).unwrap().current, 0);
        let ev = events(&mut w);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, UiEventKind::ValueChanged(UiValue::Index(0)));
    }

    #[test]
    fn breadcrumbs_sync_builds_crumbs_labels_and_separators() {
        let mut w = world();
        let e = crumbs(&mut w, &["Root", "Settings", "Audio"], 2);
        sync_breadcrumb_children(&mut w);
        assert_eq!(crumb_labels(&w, e), vec!["Root", "Settings", "Audio"]);
        assert_eq!(separators(&w, e), 2);
        let kids: Vec<Entity> = w
            .get::<Children>(e)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        let primary: Vec<usize> = kids
            .into_iter()
            .filter_map(|c| {
                let index = w.get::<CrumbIndex>(c)?.0;
                w.get::<Classes>(c)
                    .filter(|c| c.0.iter().any(|c| c == "primary"))
                    .map(|_| index)
            })
            .collect();
        assert_eq!(primary, vec![2]);
        // A second sync only refreshes: renaming a segment keeps the same crumb entities.
        let before: Vec<Entity> = w
            .get::<Children>(e)
            .map(|c| c.iter().filter(|c| w.get::<CrumbIndex>(*c).is_some()).collect())
            .unwrap_or_default();
        w.get_mut::<Breadcrumbs>(e).unwrap().segments[1] = "Video".into();
        w.get_mut::<Breadcrumbs>(e).unwrap().current = 1;
        sync_breadcrumb_children(&mut w);
        let after: Vec<Entity> = w
            .get::<Children>(e)
            .map(|c| c.iter().filter(|c| w.get::<CrumbIndex>(*c).is_some()).collect())
            .unwrap_or_default();
        assert_eq!(before, after);
        assert_eq!(crumb_labels(&w, e), vec!["Root", "Video", "Audio"]);
        // Shrinking the path rebuilds the crumbs.
        w.get_mut::<Breadcrumbs>(e).unwrap().segments.pop();
        sync_breadcrumb_children(&mut w);
        assert_eq!(crumb_labels(&w, e), vec!["Root", "Video"]);
        assert_eq!(separators(&w, e), 1);
    }

    #[test]
    fn breadcrumbs_empty_path_has_no_crumbs() {
        let mut w = world();
        let e = crumbs(&mut w, &[], 0);
        assert!(!navigate_breadcrumbs(&mut w, e, 0));
        sync_breadcrumb_children(&mut w);
        assert_eq!(crumb_labels(&w, e), Vec::<String>::new());
        assert_eq!(separators(&w, e), 0);
    }

    #[test]
    fn breadcrumbs_agent_values_reach_the_widget() {
        let mut w = world();
        let e = crumbs(&mut w, &["Root", "Settings"], 0);
        assert_eq!(apply_widget_value(&mut w, e, &UiValue::Index(1)), Some(true));
        assert_eq!(apply_widget_value(&mut w, e, &UiValue::Index(1)), Some(false));
        assert_eq!(apply_widget_value(&mut w, e, &UiValue::Index(9)), Some(false));
        assert_eq!(apply_widget_value(&mut w, e, &UiValue::Bool(true)), None);
        assert_eq!(widget_value(&w, e), Some(serde_json::json!(1)));
        assert_eq!(widget_kind(&w, e), Some("Breadcrumbs"));
    }
}
