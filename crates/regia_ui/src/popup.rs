// SPDX-License-Identifier: MPL-2.0
//! The popup layer (ADR-273 lane 5, gap W02): dropdown lists, tooltips and menus drawn above
//! every other root and outside every owner's clip.
//!
//! Each open popup is its own [`UiRoot`] with a high `sort_order` ([`POPUP_SORT_ORDER`] plus its
//! nesting depth), so nothing an owner clips or draws later can cover it. The root holds one
//! absolutely positioned panel ([`PopupHandle::panel`]); widgets fill the panel with their rows.
//! Interactive popups (menus, dropdown lists) are also [`InputLayer`]s on lane 4's stack, so only
//! the top popup receives pointer and navigation input, and closing one gives focus back to the
//! node that opened it. Passive popups (tooltips) take no input and no focus.
//!
//! Placement is a pure function of rectangles ([`place_popup`]): the panel goes below, above,
//! left or right of its anchor, flips to the opposite side when its side lacks room, then shifts
//! to stay inside the safe area ([`popup_bounds`]). A popup is laid out once hidden to measure its
//! size, then placed and shown, so it never flashes at the wrong place.
//!
//! An interactive popup closes on a press outside every open popup panel, on `Escape` or gamepad
//! B (the focus system's cancel, which clicks the top layer root), when its anchor despawns, and
//! when its parent popup closes. All of this runs inside `widgets::ui_widgets`, so no system is
//! added to the schedule.

use regia_app::prelude::*;

use crate::interaction::{UiEvent, UiEventKind, UiPointer};
use crate::layers::{InputLayer, LayerInputMode};
use crate::node::{Classes, ComputedNode, Focused, NodeStyle, Opacity, UiNode, UiRoot};
use crate::scale::{ScaleMode, UiDevicePolicy, UiScales};
use crate::style::{Dimension, Edges, FlexDirection, PositionType, Style};
use crate::theme::Themed;
use crate::theme_resolve::ThemeScope;

/// `sort_order` of a top-level popup root; a nested popup (a submenu) adds its depth. Far above
/// any root an app spawns by hand.
pub const POPUP_SORT_ORDER: i32 = 10_000;

/// Gap between an anchor and its popup, UI px.
pub const POPUP_GAP: f32 = 2.0;

/// Which side of its anchor a popup prefers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopupPlacement {
    /// Under the anchor, left edges aligned (dropdowns, menu bar menus, context menus).
    #[default]
    Below,
    /// Over the anchor, left edges aligned.
    Above,
    /// Left of the anchor, top edges aligned.
    Left,
    /// Right of the anchor, top edges aligned (submenus).
    Right,
}

impl PopupPlacement {
    /// The protocol name (`below`, `above`, `left`, `right`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Below => "below",
            Self::Above => "above",
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    /// Parses a protocol name.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "below" => Some(Self::Below),
            "above" => Some(Self::Above),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            _ => None,
        }
    }

    /// The other side of the same axis.
    pub fn opposite(self) -> Self {
        match self {
            Self::Below => Self::Above,
            Self::Above => Self::Below,
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

/// Where [`place_popup`] put a popup.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacedPopup {
    /// The popup rect (x, y, w, h), in the anchor's coordinates.
    pub rect: [f32; 4],
    /// The side actually used.
    pub placement: PopupPlacement,
    /// Whether the popup moved to the opposite side for lack of room.
    pub flipped: bool,
    /// Whether the popup was moved to stay inside the bounds.
    pub shifted: bool,
}

/// Places a popup of `size` next to `anchor` (x, y, w, h) on the `preferred` side, `gap` away,
/// inside `bounds` (x, y, w, h). The popup flips to the opposite side when its side has less room
/// than the popup needs and the opposite side has more; it then shifts along both axes to stay
/// inside `bounds` (a popup larger than `bounds` keeps its top-left corner on the bounds' corner).
/// Below and above align the left edges; left and right align the top edges. A point anchor is a
/// rect of zero size.
pub fn place_popup(
    anchor: [f32; 4],
    size: [f32; 2],
    preferred: PopupPlacement,
    bounds: [f32; 4],
    gap: f32,
) -> PlacedPopup {
    let [ax, ay, aw, ah] = anchor;
    let [w, h] = [size[0].max(0.0), size[1].max(0.0)];
    let [bx, by, bw, bh] = bounds;
    let room = |side: PopupPlacement| match side {
        PopupPlacement::Below => by + bh - (ay + ah + gap),
        PopupPlacement::Above => ay - gap - by,
        PopupPlacement::Right => bx + bw - (ax + aw + gap),
        PopupPlacement::Left => ax - gap - bx,
    };
    let need = match preferred {
        PopupPlacement::Below | PopupPlacement::Above => h,
        PopupPlacement::Left | PopupPlacement::Right => w,
    };
    let mut side = preferred;
    let mut flipped = false;
    if room(side) < need && room(side.opposite()) > room(side) {
        side = side.opposite();
        flipped = true;
    }
    let (x, y) = match side {
        PopupPlacement::Below => (ax, ay + ah + gap),
        PopupPlacement::Above => (ax, ay - gap - h),
        PopupPlacement::Right => (ax + aw + gap, ay),
        PopupPlacement::Left => (ax - gap - w, ay),
    };
    let fit = |v: f32, len: f32, start: f32, extent: f32| {
        // `clamp` panics when a bound is NaN: a NaN bound, size or position leaves the popup at the corner.
        if start.is_nan() || extent.is_nan() || len.is_nan() || v.is_nan() || len >= extent {
            start
        } else {
            v.clamp(start, start + extent - len)
        }
    };
    let (fx, fy) = (fit(x, w, bx, bw), fit(y, h, by, bh));
    PlacedPopup {
        rect: [fx, fy, w, h],
        placement: side,
        flipped,
        shifted: (fx - x).abs() > f32::EPSILON || (fy - y).abs() > f32::EPSILON,
    }
}

/// The area popups stay inside, physical px (x, y, w, h): the window minus the platform safe area
/// (and a device policy's title-safe margin), the same inset a [`crate::SafeAreaPadding`] root
/// gets. Popups always respect it, whether their owner's root opted in or not.
pub fn popup_bounds(world: &World) -> [f32; 4] {
    let size = world
        .get_resource::<UiScales>()
        .map_or([0.0, 0.0], |s| s.window_size);
    let safe = crate::output::forced_safe_area(world)
        .or_else(|| world.get_resource::<regia_platform::SafeArea>().copied());
    let mut pad = [0.0f32; 4];
    if let Some(safe) = safe {
        pad = [
            safe.left as f32,
            safe.top as f32,
            safe.right as f32,
            safe.bottom as f32,
        ];
        if let Some(policy) = world.get_resource::<UiDevicePolicy>() {
            for (p, e) in pad.iter_mut().zip(policy.title_safe_insets(size)) {
                *p = p.max(e);
            }
        }
    }
    [
        pad[0],
        pad[1],
        (size[0] - pad[0] - pad[2]).max(0.0),
        (size[1] - pad[1] - pad[3]).max(0.0),
    ]
}

/// What a popup is for (agents read it; widgets find their own popups by it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PopupKind {
    /// A plain popup opened by code or `ui.open_popup`.
    #[default]
    Custom,
    /// A [`crate::widgets::Dropdown`]'s option list.
    Dropdown,
    /// A [`crate::widgets::Tooltip`].
    Tooltip,
    /// A menu or submenu ([`crate::menu`]).
    Menu,
}

impl PopupKind {
    /// The protocol name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Custom => "custom",
            Self::Dropdown => "dropdown",
            Self::Tooltip => "tooltip",
            Self::Menu => "menu",
        }
    }
}

/// What to open: see [`open_popup`].
#[derive(Debug, Clone, PartialEq)]
pub struct PopupRequest {
    /// What the popup is for.
    pub kind: PopupKind,
    /// The node to place against. The popup closes when it despawns.
    pub anchor: Option<Entity>,
    /// A point anchor (physical px), used when `anchor` is `None` (context menus open at the
    /// pointer).
    pub point: Option<[f32; 2]>,
    /// Preferred side.
    pub placement: PopupPlacement,
    /// The node focus returns to on close (usually the anchor).
    pub opener: Option<Entity>,
    /// The popup this one belongs to (a submenu's menu); closing the parent closes it.
    pub parent: Option<Entity>,
    /// Interactive: an input layer that takes focus and closes on outside presses, `Escape` and
    /// gamepad B. A passive popup (a tooltip) takes no input.
    pub interactive: bool,
    /// Minimum panel width, UI px (a dropdown list is at least as wide as the dropdown).
    pub min_width: f32,
    /// Theme classes of the panel (`["Menu"]`, `["Tooltip"]`).
    pub classes: Vec<String>,
}

impl Default for PopupRequest {
    fn default() -> Self {
        Self {
            kind: PopupKind::Custom,
            anchor: None,
            point: None,
            placement: PopupPlacement::Below,
            opener: None,
            parent: None,
            interactive: true,
            min_width: 0.0,
            classes: vec!["Menu".to_owned()],
        }
    }
}

/// An open popup: the component on its root.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct Popup {
    /// What the popup is for.
    pub kind: PopupKind,
    /// The node it is placed against.
    pub anchor: Option<Entity>,
    /// The point it is placed at when there is no anchor (physical px).
    pub point: Option<[f32; 2]>,
    /// Preferred side.
    pub placement: PopupPlacement,
    /// Where focus returns on close.
    pub opener: Option<Entity>,
    /// The popup this one belongs to.
    pub parent: Option<Entity>,
    /// Takes input (an input layer) or not.
    pub interactive: bool,
    /// The panel node that holds the popup's content.
    pub panel: Entity,
    /// The last placement, once the panel was measured (`None` while hidden for measuring).
    pub placed: Option<PlacedPopup>,
}

/// Marks a popup's panel (the absolutely positioned content node).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopupPanel {
    /// The popup root.
    pub popup: Entity,
}

/// The open popups, in open order (a submenu after its menu).
#[derive(Resource, Debug, Default)]
pub struct PopupStack {
    open: Vec<Entity>,
}

impl PopupStack {
    /// Open popup roots, oldest first.
    pub fn popups(&self) -> &[Entity] {
        &self.open
    }

    /// The newest popup, if any.
    pub fn top(&self) -> Option<Entity> {
        self.open.last().copied()
    }

    /// Whether `root` is an open popup.
    pub fn contains(&self, root: Entity) -> bool {
        self.open.contains(&root)
    }
}

/// The two nodes [`open_popup`] spawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopupHandle {
    /// The popup's root ([`UiRoot`] + [`Popup`], plus [`InputLayer`] when interactive).
    pub root: Entity,
    /// The panel to put content in.
    pub panel: Entity,
}

/// Pointer bookkeeping for outside presses.
#[derive(Resource, Debug, Default)]
struct PopupPointerState {
    was_down: bool,
}

/// The [`UiRoot`] above `entity` (or `entity` itself), if any.
pub(crate) fn root_of(world: &World, entity: Entity) -> Option<Entity> {
    let mut current = entity;
    for _ in 0..4096 {
        if world.get::<UiRoot>(current).is_some() {
            return Some(current);
        }
        current = world.get::<ChildOf>(current)?.0;
    }
    None
}

/// The nearest [`ThemeScope`] at or above `entity`.
fn theme_scope_of(world: &World, entity: Entity) -> Option<ThemeScope> {
    let mut current = entity;
    for _ in 0..4096 {
        if let Some(scope) = world.get::<ThemeScope>(current) {
            return Some(scope.clone());
        }
        current = world.get::<ChildOf>(current)?.0;
    }
    None
}

/// The newest open interactive popup (the one that has input), skipping passive ones such as
/// tooltips.
pub fn top_interactive(world: &World) -> Option<Entity> {
    world
        .get_resource::<PopupStack>()?
        .open
        .iter()
        .rev()
        .copied()
        .find(|p| world.get::<Popup>(*p).is_some_and(|popup| popup.interactive))
}

/// The open popup containing `entity` (the innermost one), if any.
pub fn popup_of(world: &World, entity: Entity) -> Option<Entity> {
    let root = root_of(world, entity)?;
    world.get::<Popup>(root).map(|_| root)
}

/// Opens a popup and returns its root and panel. The panel is laid out hidden first and shown,
/// placed, on the next frame. An interactive popup becomes the top input layer on the next frame
/// (focusing its [`crate::DefaultFocus`] or first focusable node).
pub fn open_popup(world: &mut World, request: PopupRequest) -> PopupHandle {
    world.init_resource::<PopupStack>();
    let owner_root = request
        .anchor
        .or(request.opener)
        .and_then(|e| root_of(world, e))
        .and_then(|r| world.get::<UiRoot>(r).cloned());
    let (target, scale_mode) = owner_root.map_or((Default::default(), ScaleMode::default()), |r| {
        (r.target, r.scale_mode)
    });
    let depth = {
        let mut depth = 0;
        let mut parent = request.parent;
        while let Some(p) = parent
            && depth < 64
        {
            depth += 1;
            parent = world.get::<Popup>(p).and_then(|popup| popup.parent);
        }
        depth
    };
    // First guess at the panel position: at the anchor, refined once the panel is measured.
    let (scale, start) = match request.anchor.and_then(|a| world.get::<ComputedNode>(a)) {
        Some(node) => (node.scale.max(0.001), [node.rect[0], node.rect[1] + node.rect[3]]),
        None => (
            world
                .get_resource::<UiScales>()
                .map_or(1.0, |s| s.scale_of(&scale_mode))
                .max(0.001),
            request.point.unwrap_or([0.0, 0.0]),
        ),
    };
    let root = world
        .spawn((
            UiRoot {
                target,
                scale_mode,
                sort_order: POPUP_SORT_ORDER + depth,
            },
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id();
    let panel = world
        .spawn((
            UiNode,
            PopupPanel { popup: root },
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                direction: FlexDirection::Column,
                inset: Edges {
                    left: Dimension::Points(start[0] / scale),
                    top: Dimension::Points(start[1] / scale),
                    right: Dimension::Auto,
                    bottom: Dimension::Auto,
                },
                min_size: [Dimension::Points(request.min_width.max(0.0)), Dimension::Auto],
                padding: Edges::px(4.0),
                ..Default::default()
            }),
            Classes(request.classes.clone()),
            Themed::default(),
            // Hidden until measured and placed.
            Opacity(0.0),
            ChildOf(root),
        ))
        .id();
    // A popup lives outside its owner's subtree: carry the owner's theme scope over.
    if let Some(scope) = request
        .anchor
        .or(request.opener)
        .and_then(|e| theme_scope_of(world, e))
    {
        world.entity_mut(root).insert(scope);
    }
    world.entity_mut(root).insert(Popup {
        kind: request.kind,
        anchor: request.anchor,
        point: request.point,
        placement: request.placement,
        opener: request.opener,
        parent: request.parent,
        interactive: request.interactive,
        panel,
        placed: None,
    });
    if request.interactive {
        world.entity_mut(root).insert(InputLayer {
            mode: LayerInputMode::UiOnly,
        });
    }
    world.resource_mut::<PopupStack>().open.push(root);
    world.resource_mut::<crate::layout::LayoutDirty>().mark();
    PopupHandle { root, panel }
}

/// Closes `root` and every popup that belongs to it (its submenus), despawning them, updates the
/// input layer stack at once and gives focus back to `root`'s opener when it can take focus.
/// Returns the closed roots (children first); empty when `root` is not an open popup.
pub fn close_popup(world: &mut World, root: Entity) -> Vec<Entity> {
    world.init_resource::<PopupStack>();
    if !world.resource::<PopupStack>().contains(root) {
        return Vec::new();
    }
    let mut doomed = vec![root];
    let mut i = 0;
    while i < doomed.len() {
        let parent = doomed[i];
        let children: Vec<Entity> = world
            .resource::<PopupStack>()
            .open
            .iter()
            .copied()
            .filter(|p| {
                world
                    .get::<Popup>(*p)
                    .is_some_and(|popup| popup.parent == Some(parent))
            })
            .collect();
        for child in children {
            if !doomed.contains(&child) {
                doomed.push(child);
            }
        }
        i += 1;
    }
    doomed.reverse();
    let opener = world.get::<Popup>(root).and_then(|p| p.opener);
    let interactive = world.get::<Popup>(root).is_some_and(|p| p.interactive);
    for popup in &doomed {
        if let Ok(entity) = world.get_entity_mut(*popup) {
            entity.despawn();
        }
    }
    world
        .resource_mut::<PopupStack>()
        .open
        .retain(|p| !doomed.contains(p));
    world.resource_mut::<crate::layout::LayoutDirty>().mark();
    if interactive {
        crate::layers::sync_layers(world);
        if let Some(opener) = opener
            && world.get::<crate::focus::Focusable>(opener).is_some()
            && !crate::node::is_disabled(world, opener)
        {
            set_focus(world, Some(opener));
        }
    }
    doomed
}

/// Closes every open popup. Returns the closed roots.
pub fn close_all_popups(world: &mut World) -> Vec<Entity> {
    world.init_resource::<PopupStack>();
    let roots: Vec<Entity> = world
        .resource::<PopupStack>()
        .open
        .iter()
        .copied()
        .filter(|p| world.get::<Popup>(*p).is_none_or(|popup| popup.parent.is_none()))
        .collect();
    let mut closed = Vec::new();
    for root in roots {
        closed.extend(close_popup(world, root));
    }
    closed
}

/// Moves focus the way `focus::set_focus` does (the `Focused` marker, `FocusChanged` events).
pub(crate) fn set_focus(world: &mut World, next: Option<Entity>) {
    world.init_resource::<crate::focus::FocusState>();
    let prev = world.resource::<crate::focus::FocusState>().focused;
    if prev == next {
        return;
    }
    if let Some(p) = prev
        && let Ok(mut entity) = world.get_entity_mut(p)
    {
        entity.remove::<Focused>();
    }
    if let Some(n) = next
        && let Ok(mut entity) = world.get_entity_mut(n)
    {
        entity.insert(Focused);
    }
    if let Some(p) = prev {
        world.write_message(UiEvent::new(p, UiEventKind::FocusChanged(false)));
    }
    if let Some(n) = next {
        world.write_message(UiEvent::new(n, UiEventKind::FocusChanged(true)));
    }
    world.resource_mut::<crate::focus::FocusState>().focused = next;
}

/// Drops stack entries whose root despawned without [`close_popup`].
fn prune_stack(world: &mut World) {
    let stale: Vec<Entity> = world
        .resource::<PopupStack>()
        .open
        .iter()
        .copied()
        .filter(|p| world.get::<Popup>(*p).is_none())
        .collect();
    if !stale.is_empty() {
        world
            .resource_mut::<PopupStack>()
            .open
            .retain(|p| !stale.contains(p));
    }
}

/// Whether `p` lies inside `rect` (x, y, w, h).
fn contains(rect: [f32; 4], p: [f32; 2]) -> bool {
    p[0] >= rect[0] && p[1] >= rect[1] && p[0] <= rect[0] + rect[2] && p[1] <= rect[1] + rect[3]
}

/// Closing rules, run first in `ui_widgets`: anchors that despawned, the cancel action (a click on
/// the top layer root) and presses outside every popup panel.
pub(crate) fn popup_input(world: &mut World) {
    let _span = tracing::trace_span!("ui_popup_input").entered();
    world.init_resource::<PopupStack>();
    world.init_resource::<PopupPointerState>();
    prune_stack(world);
    let (down, pos) = world.get_resource::<UiPointer>().map_or((false, None), |p| {
        (p.down() || p.secondary_down || p.middle_down, p.position())
    });
    let pressed_now = {
        let mut state = world.resource_mut::<PopupPointerState>();
        let edge = down && !state.was_down;
        state.was_down = down;
        edge
    };
    let open: Vec<Entity> = world.resource::<PopupStack>().open.clone();
    if open.is_empty() {
        return;
    }
    // Anchors that despawned close their popup.
    for root in &open {
        let gone = world
            .get::<Popup>(*root)
            .and_then(|p| p.anchor)
            .is_some_and(|a| world.get_entity(a).is_err());
        if gone {
            close_popup(world, *root);
        }
    }
    // Cancel (Escape, gamepad B): the focus system clicks the top layer root.
    let cancels: Vec<Entity> = world
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .filter(|e| e.kind == UiEventKind::Click)
        .map(|e| e.target)
        .collect();
    for target in cancels {
        let interactive = world.get::<Popup>(target).is_some_and(|p| p.interactive);
        if interactive {
            close_popup(world, target);
        }
    }
    // A press outside: close the popups above the innermost one under the pointer (all of them
    // when it is under none).
    if pressed_now && let Some(p) = pos {
        let open: Vec<Entity> = world.resource::<PopupStack>().open.clone();
        let hit = open.iter().rposition(|root| {
            world
                .get::<Popup>(*root)
                .and_then(|popup| world.get::<ComputedNode>(popup.panel))
                .is_some_and(|node| contains(node.rect, p))
        });
        let keep = hit.map_or(0, |i| i + 1);
        for root in open.iter().skip(keep).rev() {
            let interactive = world.get::<Popup>(*root).is_some_and(|popup| popup.interactive);
            if interactive {
                close_popup(world, *root);
            }
        }
    }
}

/// Places every open popup that has been measured, and shows it. Runs last in `ui_widgets`, so
/// the placement reaches this frame's layout and the popup is drawn in place from its first
/// visible frame.
pub(crate) fn place_popups(world: &mut World) {
    let _span = tracing::trace_span!("ui_popup_place").entered();
    world.init_resource::<PopupStack>();
    prune_stack(world);
    let open: Vec<Entity> = world.resource::<PopupStack>().open.clone();
    if open.is_empty() {
        return;
    }
    let bounds = popup_bounds(world);
    let mut moved = false;
    for root in open {
        let Some(popup) = world.get::<Popup>(root).cloned() else {
            continue;
        };
        let Some(panel) = world.get::<ComputedNode>(popup.panel).copied() else {
            continue;
        };
        if panel.rect[2] <= 0.0 && panel.rect[3] <= 0.0 {
            continue;
        }
        let scale = panel.scale.max(0.001);
        // A node anchor keeps a small gap; a point anchor (the pointer) opens right at it.
        let (anchor, gap) = match popup.anchor {
            Some(a) => match world.get::<ComputedNode>(a) {
                Some(node) => (node.rect, POPUP_GAP * scale),
                None => continue,
            },
            None => {
                let p = popup.point.unwrap_or([0.0, 0.0]);
                ([p[0], p[1], 0.0, 0.0], 0.0)
            }
        };
        let placed = place_popup(
            anchor,
            [panel.rect[2], panel.rect[3]],
            popup.placement,
            bounds,
            gap,
        );
        let want = [placed.rect[0] / scale, placed.rect[1] / scale];
        if let Some(mut style) = world.get_mut::<NodeStyle>(popup.panel) {
            let current = [style.0.inset.left, style.0.inset.top];
            let differs = |d: Dimension, v: f32| match d {
                Dimension::Points(p) => (p - v).abs() > 0.25,
                _ => true,
            };
            if differs(current[0], want[0]) || differs(current[1], want[1]) {
                style.0.inset.left = Dimension::Points(want[0]);
                style.0.inset.top = Dimension::Points(want[1]);
                moved = true;
            }
        }
        if popup.placed.is_none() {
            if let Ok(mut entity) = world.get_entity_mut(popup.panel) {
                entity.remove::<Opacity>();
            }
            moved = true;
        }
        if popup.placed != Some(placed)
            && let Some(mut p) = world.get_mut::<Popup>(root)
        {
            p.placed = Some(placed);
        }
    }
    if moved {
        world.resource_mut::<crate::layout::LayoutDirty>().mark();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: [f32; 4] = [0.0, 0.0, 1280.0, 720.0];

    #[test]
    fn below_fits_and_aligns_left_edges() {
        let p = place_popup(
            [100.0, 100.0, 200.0, 40.0],
            [240.0, 300.0],
            PopupPlacement::Below,
            SCREEN,
            2.0,
        );
        assert_eq!(p.rect, [100.0, 142.0, 240.0, 300.0]);
        assert_eq!(p.placement, PopupPlacement::Below);
        assert!(!p.flipped && !p.shifted);
    }

    #[test]
    fn near_the_bottom_it_flips_above() {
        let p = place_popup(
            [100.0, 640.0, 200.0, 40.0],
            [240.0, 300.0],
            PopupPlacement::Below,
            SCREEN,
            2.0,
        );
        assert_eq!(p.placement, PopupPlacement::Above);
        assert!(p.flipped);
        assert_eq!(p.rect, [100.0, 338.0, 240.0, 300.0]);
    }

    #[test]
    fn above_flips_below_at_the_top() {
        let p = place_popup(
            [100.0, 10.0, 200.0, 40.0],
            [240.0, 100.0],
            PopupPlacement::Above,
            SCREEN,
            0.0,
        );
        assert_eq!(p.placement, PopupPlacement::Below);
        assert_eq!(p.rect[1], 50.0);
    }

    #[test]
    fn right_flips_left_at_the_right_edge() {
        let p = place_popup(
            [1100.0, 200.0, 150.0, 30.0],
            [200.0, 120.0],
            PopupPlacement::Right,
            SCREEN,
            0.0,
        );
        assert_eq!(p.placement, PopupPlacement::Left);
        assert_eq!(p.rect, [900.0, 200.0, 200.0, 120.0]);
        let l = place_popup(
            [20.0, 200.0, 150.0, 30.0],
            [200.0, 120.0],
            PopupPlacement::Left,
            SCREEN,
            0.0,
        );
        assert_eq!(l.placement, PopupPlacement::Right);
        assert_eq!(l.rect[0], 170.0);
    }

    #[test]
    fn a_point_at_the_right_edge_shifts_inside_the_bounds() {
        let safe = [0.0, 0.0, 1240.0, 720.0];
        let p = place_popup(
            [1270.0, 100.0, 0.0, 0.0],
            [200.0, 150.0],
            PopupPlacement::Below,
            safe,
            0.0,
        );
        assert!(p.shifted && !p.flipped);
        assert_eq!(p.rect, [1040.0, 100.0, 200.0, 150.0]);
        assert!(p.rect[0] + p.rect[2] <= safe[0] + safe[2]);
    }

    #[test]
    fn the_cross_axis_shifts_too() {
        // A submenu near the bottom keeps its side and moves up.
        let p = place_popup(
            [300.0, 680.0, 150.0, 30.0],
            [200.0, 200.0],
            PopupPlacement::Right,
            SCREEN,
            0.0,
        );
        assert_eq!(p.placement, PopupPlacement::Right);
        assert_eq!(p.rect, [450.0, 520.0, 200.0, 200.0]);
    }

    #[test]
    fn no_flip_when_the_other_side_is_worse() {
        // 300 px tall below a node at y=300 in a 720 screen: 378 below, 298 above.
        let p = place_popup(
            [0.0, 300.0, 100.0, 40.0],
            [100.0, 500.0],
            PopupPlacement::Below,
            SCREEN,
            2.0,
        );
        assert_eq!(p.placement, PopupPlacement::Below);
        assert!(!p.flipped);
        // Taller than the room: shifted up so its bottom meets the screen bottom.
        assert_eq!(p.rect[1], 220.0);
    }

    #[test]
    fn larger_than_the_bounds_sticks_to_the_corner() {
        let bounds = [10.0, 20.0, 300.0, 200.0];
        let p = place_popup(
            [50.0, 50.0, 10.0, 10.0],
            [400.0, 400.0],
            PopupPlacement::Below,
            bounds,
            0.0,
        );
        assert_eq!([p.rect[0], p.rect[1]], [10.0, 20.0]);
    }

    #[test]
    fn placement_names_round_trip() {
        for p in [
            PopupPlacement::Below,
            PopupPlacement::Above,
            PopupPlacement::Left,
            PopupPlacement::Right,
        ] {
            assert_eq!(PopupPlacement::parse(p.name()), Some(p));
            assert_eq!(p.opposite().opposite(), p);
        }
        assert_eq!(PopupPlacement::parse("up"), None);
    }
}
