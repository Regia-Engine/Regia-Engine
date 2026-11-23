// SPDX-License-Identifier: MPL-2.0
//! Scroll views (drag with touch inertia, wheel, scrollbars) and virtualised list views
//! (`specs/18` § 1). A `ScrollView` entity holds one "content" child; [`ScrollOffset`] translates
//! the content. A [`ListView`] recycles a pool of item rows so 1,000+ items scroll at the cost of
//! a dozen entities.
//!
//! Scroll views nest (ADR-520): the wheel goes to the innermost view under the pointer and a drag
//! to the innermost view around the pressed node; what a view cannot use on an axis passes to the
//! view around it ([`ScrollChaining`]). A view can scroll one axis only ([`ScrollAxes`]), bounce at
//! its edges ([`Overscroll`]) and smooth the wheel. The motion math lives in
//! [`crate::scroll_motion`].

use regia_app::prelude::*;

use crate::interaction::{UiEvent, UiEventKind, UiPointer};
use crate::layout::ScrollDirty;
use crate::list::ListRow;
use crate::node::{ClipChildren, ComputedNode, Interactable, NodeStyle, UiNode};
use crate::scroll_motion::{
    ChainLink, DragSamples, FLING_WINDOW, MIN_FLING_SPEED, SPRING_OMEGA, SPRING_REST, coast, distribute,
    overscroll_amount, spring_step,
};
pub use crate::scroll_motion::{Overscroll, ScrollAxes, ScrollChaining, ScrollInput};
use crate::scrollbar::ScrollbarPart;
use crate::style::Style;

/// Scroll position of a scroll view's content (physical px; positive scrolls content up/left).
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct ScrollOffset(pub [f32; 2]);

/// Scroll inertia state.
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct ScrollVelocity {
    /// px per second.
    pub v: [f32; 2],
}

/// A scrollable container: drag (touch and mouse), mouse wheel, inertia after release, scrollbar
/// visuals. Content overflows the box; add `ClipChildren` (or `overflow: clip`) to clip, or spawn
/// it with [`scroll_container`]. Adding a `ScrollView` also adds [`ScrollOffset`],
/// [`ScrollVelocity`] and [`ScrollMotion`].
#[derive(Component, Debug, Clone, PartialEq)]
#[require(ScrollOffset, ScrollVelocity, ScrollMotion)]
pub struct ScrollView {
    /// Inertia time constant (seconds); higher coasts further. 0 disables inertia.
    pub inertia: f32,
    /// Whether the vertical scrollbar draws.
    pub show_scrollbar: bool,
    /// Frame-to-frame drag delta (set by the scroll system).
    pub(crate) drag_delta: [f32; 2],
    /// Axes the view scrolls; deltas on the other axis pass to the enclosing view.
    pub axes: ScrollAxes,
    /// Edge behavior: stop, or rubber-band and spring back.
    pub overscroll: Overscroll,
    /// Whether deltas this view cannot use pass to the enclosing scroll view.
    pub chaining: ScrollChaining,
    /// Wheel smoothing time constant (seconds): the view eases toward the wheel's target instead
    /// of jumping. 0 (the default) jumps.
    pub wheel_smoothing: f32,
}

impl Default for ScrollView {
    fn default() -> Self {
        Self {
            inertia: 0.18,
            show_scrollbar: true,
            drag_delta: [0.0; 2],
            axes: ScrollAxes::Both,
            overscroll: Overscroll::Clamp,
            chaining: ScrollChaining::Chain,
            wheel_smoothing: 0.0,
        }
    }
}

impl ScrollView {
    /// A view that scrolls top to bottom only.
    pub fn vertical() -> Self {
        Self {
            axes: ScrollAxes::Vertical,
            ..Default::default()
        }
    }

    /// A view that scrolls left to right only (carousels, tab strips).
    pub fn horizontal() -> Self {
        Self {
            axes: ScrollAxes::Horizontal,
            ..Default::default()
        }
    }

    /// The same view with another edge behavior.
    pub fn with_overscroll(mut self, overscroll: Overscroll) -> Self {
        self.overscroll = overscroll;
        self
    }

    /// The same view, keeping every delta to itself ([`ScrollChaining::Contain`]).
    pub fn contained(mut self) -> Self {
        self.chaining = ScrollChaining::Contain;
        self
    }

    /// The same view with wheel smoothing (`seconds`, 0 jumps).
    pub fn with_wheel_smoothing(mut self, seconds: f32) -> Self {
        self.wheel_smoothing = seconds.max(0.0);
        self
    }
}

/// A clipping scroll container: a UI node with `style`, `view`, [`ClipChildren`] and an
/// [`Interactable`] so drags and the wheel over its empty space reach it. Spawn the content as
/// its children.
pub fn scroll_container(
    style: Style,
    view: ScrollView,
) -> (UiNode, NodeStyle, ClipChildren, Interactable, ScrollView) {
    (
        UiNode,
        NodeStyle::new(style),
        ClipChildren,
        Interactable::default(),
        view,
    )
}

/// Gesture state of a scroll view (drag samples, wheel target, fling count). Added with every
/// [`ScrollView`]; read it through the accessors.
#[derive(Component, Debug, Clone, Default)]
pub struct ScrollMotion {
    samples: DragSamples,
    origin: f64,
    dragging: bool,
    held: bool,
    wheel_target: Option<[f32; 2]>,
    last_offset: [f32; 2],
    flings: u32,
    /// A press stopped the return spring. A release without a drag does not restart it.
    spring_paused: bool,
}

impl ScrollMotion {
    /// Whether a drag is moving the view now.
    pub fn dragging(&self) -> bool {
        self.dragging
    }

    /// Whether a press inside the view holds it still (it stops a fling or a spring).
    pub fn held(&self) -> bool {
        self.held
    }

    /// Where wheel smoothing is heading, while it eases.
    pub fn wheel_target(&self) -> Option<[f32; 2]> {
        self.wheel_target
    }

    /// Flings started since the view was created (a drag released fast enough, or an agent fling).
    pub fn flings(&self) -> u32 {
        self.flings
    }

    /// Counts a fling started from outside the scroll system (the agent's `ui.scroll` fling).
    pub fn note_fling(&mut self) {
        self.flings = self.flings.saturating_add(1);
    }

    /// Drops the wheel target and any drag in progress (an external jump of the offset).
    pub fn cancel(&mut self) {
        self.wheel_target = None;
        self.dragging = false;
    }
}

/// Largest in-range offset per axis (`content - viewport`, at least 0).
pub fn max_offset(computed: &ComputedNode) -> [f32; 2] {
    [
        (computed.content_size[0] - computed.rect[2]).max(0.0),
        (computed.content_size[1] - computed.rect[3]).max(0.0),
    ]
}

/// Whether a scroll view has come to rest: no fling, drag, wheel easing or overscroll.
pub fn is_settled(
    computed: &ComputedNode,
    offset: &ScrollOffset,
    velocity: &ScrollVelocity,
    motion: &ScrollMotion,
) -> bool {
    let max = max_offset(computed);
    velocity.v == [0.0; 2]
        && !motion.dragging
        && motion.wheel_target.is_none()
        && overscroll_amount(offset.0[0], max[0]) == 0.0
        && overscroll_amount(offset.0[1], max[1]) == 0.0
}

/// The chain link [`distribute`] sees for one view at `offset`.
pub fn chain_link(computed: &ComputedNode, offset: [f32; 2], view: &ScrollView) -> ChainLink {
    ChainLink {
        offset,
        max: max_offset(computed),
        viewport: [computed.rect[2], computed.rect[3]],
        axes: view.axes,
        overscroll: view.overscroll,
        chaining: view.chaining,
    }
}

/// The scroll views around `start` (itself included when it is one), innermost first.
/// `is_view` says whether an entity is a scroll view; `parent` walks up the hierarchy.
pub fn scroll_chain(
    start: Entity,
    is_view: impl Fn(Entity) -> bool,
    parent: impl Fn(Entity) -> Option<Entity>,
) -> Vec<Entity> {
    let mut chain = Vec::new();
    let mut current = Some(start);
    // Hierarchies are trees; the bound only guards against a corrupt one.
    for _ in 0..4096 {
        let Some(entity) = current else {
            break;
        };
        if is_view(entity) {
            chain.push(entity);
        }
        current = parent(entity);
    }
    chain
}

/// Clamp helper: keeps offset within `[0, content - viewport]` per axis.
fn clamp_offset(offset: [f32; 2], viewport: [f32; 2], content: [f32; 2]) -> [f32; 2] {
    [
        offset[0].clamp(0.0, (content[0] - viewport[0]).max(0.0)),
        offset[1].clamp(0.0, (content[1] - viewport[1]).max(0.0)),
    ]
}

/// How far a view whose rect is `view` must scroll (physical px, per axis) to show `node` fully;
/// a node larger than the view lines up at its start edge.
pub fn focus_shift(view: [f32; 4], node: [f32; 4]) -> [f32; 2] {
    let mut shift = [0.0; 2];
    for (axis, out) in shift.iter_mut().enumerate() {
        let (start, end) = (view[axis], view[axis] + view[2 + axis]);
        let (n_start, n_end) = (node[axis], node[axis] + node[2 + axis]);
        if n_start < start {
            *out = n_start - start;
        } else if n_end > end {
            *out = (n_end - end).min(n_start - start);
        }
    }
    shift
}

/// Exponential decay factor for inertia over `dt`.
pub fn decay(velocity: [f32; 2], dt: f32, tau: f32) -> [f32; 2] {
    if tau <= 0.0 {
        return [0.0; 2];
    }
    let factor = (-dt / tau).exp();
    [velocity[0] * factor, velocity[1] * factor]
}

/// One data item of a [`ListView`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ListItem {
    /// Main label.
    pub label: String,
    /// Optional secondary label.
    pub subtitle: String,
}

/// A virtualised list: only the visible rows exist as entities (plus a small overscan), recycled
/// as the scroll offset moves. Rows are plain text by default ([`ListItems`] / [`ItemCount`]).
#[derive(Component, Debug, Clone, PartialEq)]
pub struct ListView {
    /// Fixed row height in UI px (uniform rows; measured at the list's scale).
    pub item_height: f32,
    /// Extra rows kept alive above and below the viewport.
    pub overscan: u32,
    /// Index of the first pooled row (changes with scrolling).
    pub(crate) first_visible: usize,
    /// Recycled row entities.
    pub(crate) pool: Vec<Entity>,
    /// Rebuild the pool next frame (spawned by the virtualise system).
    pub(crate) dirty: bool,
}

impl Default for ListView {
    fn default() -> Self {
        Self {
            item_height: 44.0,
            overscan: 2,
            first_visible: 0,
            pool: Vec::new(),
            dirty: true,
        }
    }
}

/// Row data of a [`ListView`]: when absent, `item_count` generic rows are shown.
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct ListItems(pub Vec<ListItem>);

/// How many items the list shows.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ItemCount(pub usize);

impl ListView {
    /// A list of `item_height` rows (UI px) that keeps `overscan` extra rows alive on each side.
    pub fn new(item_height: f32, overscan: u32) -> Self {
        Self {
            item_height,
            overscan,
            ..Default::default()
        }
    }

    /// Row count from `items`/`count` components on the same entity.
    pub fn count(&self, items: Option<&ListItems>, count: Option<&ItemCount>) -> usize {
        if let Some(items) = items {
            items.0.len()
        } else {
            count.map(|c| c.0).unwrap_or(0)
        }
    }
}

/// A drag, a nonzero velocity, or a wheel target ends the press pause.
fn release_pause(motion: &mut ScrollMotion, velocity: [f32; 2]) {
    if motion.dragging || velocity != [0.0; 2] || motion.wheel_target.is_some() {
        motion.spring_paused = false;
    }
}

/// Whether the view should integrate a fling or a spring this frame.
fn motion_integrates(motion: &ScrollMotion) -> bool {
    !motion.dragging && !motion.held && !motion.spring_paused
}

/// Whether `offset` is past an edge of a [`Overscroll::Bounce`] view, so its return spring is
/// running. Only then does a press pause the spring: a press on a view at rest (a tap on a row)
/// must not stop a later clamp or spring, or a list whose content shrinks after the tap keeps
/// showing the empty space past its new end.
fn spring_running(view: &ScrollView, computed: &ComputedNode, offset: [f32; 2]) -> bool {
    let max = max_offset(computed);
    view.overscroll == Overscroll::Bounce
        && (overscroll_amount(offset[0], max[0]) != 0.0 || overscroll_amount(offset[1], max[1]) != 0.0)
}

/// One step of a view's motion when nothing drags or holds it: wheel easing, then per axis the
/// overscroll spring or the fling. Returns the new offset and velocity.
pub fn step_motion(
    view: &ScrollView,
    max: [f32; 2],
    offset: [f32; 2],
    velocity: [f32; 2],
    wheel_target: &mut Option<[f32; 2]>,
    dt: f32,
) -> ([f32; 2], [f32; 2]) {
    let mut o = offset;
    let mut vel = velocity;
    if let Some(target) = *wheel_target {
        if view.wheel_smoothing > 0.0 {
            let k = 1.0 - (-dt.max(0.0) / view.wheel_smoothing).exp();
            o = [o[0] + (target[0] - o[0]) * k, o[1] + (target[1] - o[1]) * k];
            if (target[0] - o[0]).abs() < 0.5 && (target[1] - o[1]).abs() < 0.5 {
                o = target;
                *wheel_target = None;
            }
        } else {
            o = target;
            *wheel_target = None;
        }
    }
    for axis in [0, 1] {
        if !view.axes.allows(axis) {
            vel[axis] = 0.0;
        }
        let over = overscroll_amount(o[axis], max[axis]);
        if over != 0.0 && view.overscroll == Overscroll::Bounce {
            let edge = o[axis] - over;
            let (x, v) = spring_step(over, vel[axis], dt, SPRING_OMEGA);
            // A sample that changes sign would coast into the content on the next frame,
            // because overscroll is then 0. Snap to the edge instead.
            if (x.signum() != 0.0 && over.signum() != x.signum())
                || (x.abs() < SPRING_REST && v.abs() < MIN_FLING_SPEED)
            {
                o[axis] = edge;
                vel[axis] = 0.0;
            } else {
                o[axis] = edge + x;
                vel[axis] = v;
            }
        } else if vel[axis] != 0.0 {
            let (distance, mut v) = coast(vel[axis], dt, view.inertia);
            let next = o[axis] + distance;
            o[axis] = match view.overscroll {
                Overscroll::Clamp => {
                    let clamped = next.clamp(0.0, max[axis]);
                    if clamped != next {
                        v = 0.0; // hit an edge
                    }
                    clamped
                }
                Overscroll::Bounce => next,
            };
            vel[axis] = if v.abs() < MIN_FLING_SPEED { 0.0 } else { v };
        } else if over != 0.0 {
            // A clamping view whose content shrank under it.
            o[axis] = o[axis].clamp(0.0, max[axis]);
        }
    }
    (o, vel)
}

/// The fling velocity a drag leaves at release: the recent drag samples, masked to the view's
/// axes, or nothing when the view has no inertia or the drag was too slow.
fn release_velocity(view: &ScrollView, motion: &ScrollMotion, clock: f64) -> [f32; 2] {
    if view.inertia <= 0.0 {
        return [0.0; 2];
    }
    let now = (clock - motion.origin) as f32;
    let mut v = motion.samples.velocity(now, FLING_WINDOW);
    for (axis, value) in v.iter_mut().enumerate() {
        if !view.axes.allows(axis) || value.abs() < MIN_FLING_SPEED {
            *value = 0.0;
        }
    }
    v
}

type ScrollItem = (
    Entity,
    &'static ComputedNode,
    &'static mut ScrollOffset,
    &'static ScrollView,
    &'static mut ScrollVelocity,
    &'static mut ScrollMotion,
);

/// `Update`: scroll handling. Drags and the wheel route through nested views (innermost first,
/// [`distribute`]); a release flings from the recent drag samples; flings coast, overscroll
/// springs back, the wheel eases; a newly focused descendant is brought into view.
pub fn ui_scroll(
    time: Res<regia_app::RealTime>,
    mut pointer: ResMut<UiPointer>,
    mut events: MessageReader<UiEvent>,
    mut scrolls: Query<ScrollItem>,
    all_nodes: Query<&ComputedNode>,
    parents: Query<&ChildOf>,
    list_rows: Query<&ListRow>,
    bar_parts: Query<&ScrollbarPart>,
    layers: Res<crate::layers::InputLayers>,
    mut last_focus: Local<Option<Entity>>,
    mut clock: Local<f64>,
    focus_state: Res<crate::focus::FocusState>,
    mut scroll_dirty: ResMut<ScrollDirty>,
    motion_settings: (
        Option<Res<crate::theme_resolve::ActiveTheme>>,
        Option<Res<crate::scale::UiAccessibility>>,
    ),
) {
    let _span = tracing::trace_span!("ui_scroll").entered();
    // Reduced motion: the view follows the finger and the wheel, and nothing moves on its own
    // afterward (no fling, no wheel easing, no spring back; found by `a11y_audit`, ADR-3025).
    let reduced = crate::motion::reduced_motion(motion_settings.0.as_deref(), motion_settings.1.as_deref());
    let dt = time.delta().as_secs_f32();
    *clock += f64::from(dt);
    let now = *clock;
    let parent_of = |e: Entity| parents.get(e).ok().map(|p| p.0);
    let mut any_moved = false;

    // An offset someone else moved (scrollbar, agent, list scroll-to) cancels the wheel easing.
    for (_, _, offset, _, _, mut motion) in &mut scrolls {
        if offset.0 != motion.last_offset && motion.wheel_target.is_some() {
            motion.wheel_target = None;
        }
        if motion.held {
            motion.held = false;
        }
    }

    // Drags and releases, in event order. A drag that starts on a list row scrolls the list;
    // scrollbar parts are driven by the scrollbar system.
    let mut dragged: Vec<Entity> = Vec::new();
    for event in events.read() {
        if bar_parts.contains(event.target) {
            continue;
        }
        let start = list_rows.get(event.target).map_or(event.target, |r| r.list);
        match &event.kind {
            UiEventKind::Drag(delta) => {
                let chain = scroll_chain(start, |e| scrolls.contains(e), parent_of);
                let links: Vec<ChainLink> = chain
                    .iter()
                    .filter_map(|&e| scrolls.get(e).ok())
                    .map(|(_, computed, offset, view, _, _)| chain_link(computed, offset.0, view))
                    .collect();
                if links.len() != chain.len() {
                    continue;
                }
                // Content follows the pointer.
                let next = distribute(&links, [-delta[0], -delta[1]], ScrollInput::Drag);
                for (&entity, new) in chain.iter().zip(next) {
                    let Ok((_, _, mut offset, _, mut velocity, mut motion)) = scrolls.get_mut(entity) else {
                        continue;
                    };
                    if !motion.dragging {
                        motion.dragging = true;
                        motion.origin = now - f64::from(dt);
                        motion.samples.begin(0.0);
                    }
                    motion.wheel_target = None;
                    velocity.v = [0.0; 2];
                    let moved = [new[0] - offset.0[0], new[1] - offset.0[1]];
                    let origin = motion.origin;
                    motion.samples.push((now - origin) as f32, moved);
                    if new != offset.0 {
                        offset.0 = new;
                        any_moved = true;
                    }
                    dragged.push(entity);
                }
            }
            UiEventKind::DragEnd => {
                for entity in scroll_chain(start, |e| scrolls.contains(e), parent_of) {
                    if let Ok((_, _, _, view, mut velocity, mut motion)) = scrolls.get_mut(entity)
                        && motion.dragging
                    {
                        motion.dragging = false;
                        velocity.v = if reduced {
                            [0.0; 2]
                        } else {
                            release_velocity(view, &motion, now)
                        };
                        if velocity.v != [0.0; 2] {
                            motion.note_fling();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // A press inside a view holds it: a fling or spring stops under the finger.
    if let Some(pressed) = pointer.pressed.filter(|p| !bar_parts.contains(*p)) {
        let start = list_rows.get(pressed).map_or(pressed, |r| r.list);
        for entity in scroll_chain(start, |e| scrolls.contains(e), parent_of) {
            if let Ok((_, computed, offset, view, mut velocity, mut motion)) = scrolls.get_mut(entity) {
                motion.held = true;
                if spring_running(view, computed, offset.0) {
                    motion.spring_paused = true;
                }
                velocity.v = [0.0; 2];
            }
        }
    }

    // The wheel: the innermost view under the pointer (a hovered scrollbar counts as its view),
    // else the focused node's views, else (no pointer, no focus) every view as before ADR-520.
    if pointer.wheel != [0.0, 0.0] {
        let delta = [-pointer.wheel[0], -pointer.wheel[1]];
        let top_layer = layers.top_root();
        let start = if let Some(hovered) = pointer.hovered {
            Some(bar_parts.get(hovered).map_or(hovered, |p| p.view))
        } else if let Some(cursor) = pointer.cursor {
            scrolls
                .iter()
                .filter(|(e, ..)| top_layer.is_none_or(|root| crate::focus::in_scope(&parents, root, *e)))
                .filter(|(_, computed, ..)| {
                    let r = crate::draw::intersect(computed.rect, computed.clip_rect);
                    cursor[0] >= r[0]
                        && cursor[0] < r[0] + r[2]
                        && cursor[1] >= r[1]
                        && cursor[1] < r[1] + r[3]
                })
                .max_by(|a, b| a.1.z.total_cmp(&b.1.z))
                .map(|(e, ..)| e)
        } else {
            focus_state.focused
        };
        let chains: Vec<Vec<Entity>> = match start {
            Some(start) => vec![scroll_chain(start, |e| scrolls.contains(e), parent_of)],
            None if pointer.cursor.is_none() => scrolls.iter().map(|(e, ..)| vec![e]).collect(),
            None => Vec::new(),
        };
        for chain in chains {
            let links: Vec<ChainLink> = chain
                .iter()
                .filter_map(|&e| scrolls.get(e).ok())
                .map(|(_, computed, offset, view, _, motion)| {
                    let base = if view.wheel_smoothing > 0.0 && !reduced {
                        motion.wheel_target.unwrap_or(offset.0)
                    } else {
                        offset.0
                    };
                    chain_link(computed, base, view)
                })
                .collect();
            if links.len() != chain.len() {
                continue;
            }
            let next = distribute(&links, delta, ScrollInput::Wheel);
            for ((&entity, new), link) in chain.iter().zip(next).zip(&links) {
                if new == link.offset {
                    continue;
                }
                let Ok((_, _, mut offset, view, mut velocity, mut motion)) = scrolls.get_mut(entity) else {
                    continue;
                };
                velocity.v = [0.0; 2];
                if view.wheel_smoothing > 0.0 && !reduced {
                    motion.wheel_target = Some(new);
                } else if new != offset.0 {
                    offset.0 = new;
                    any_moved = true;
                }
            }
        }
        pointer.wheel = [0.0, 0.0];
    }

    // Focus is brought into view when it *moves*; a focused node must not pin the view in place
    // against the wheel.
    let focus_moved = *last_focus != focus_state.focused;
    *last_focus = focus_state.focused;
    for (entity, computed, mut offset, view, mut velocity, mut motion) in &mut scrolls {
        let viewport = [computed.rect[2], computed.rect[3]];
        let content = computed.content_size;
        let mut next = offset.0;
        // A drag the pointer no longer drives (a synthetic drag, a lost release) ends here.
        if motion.dragging && !dragged.contains(&entity) && !pointer.down() {
            motion.dragging = false;
            velocity.v = if reduced {
                [0.0; 2]
            } else {
                release_velocity(view, &motion, now)
            };
            if velocity.v != [0.0; 2] {
                motion.note_fling();
            }
        }
        release_pause(&mut motion, velocity.v);
        // The pause is for the one spring the press caught: once the view is back in range (its
        // content grew, or something else moved it), a later overscroll springs back again.
        if motion.spring_paused && !motion.held && !spring_running(view, computed, next) {
            motion.spring_paused = false;
        }
        if motion_integrates(&motion) {
            let mut target = motion.wheel_target;
            let (o, v) = if reduced {
                let calm = ScrollView {
                    wheel_smoothing: 0.0,
                    overscroll: Overscroll::Clamp,
                    ..view.clone()
                };
                let (o, _) = step_motion(&calm, max_offset(computed), next, [0.0; 2], &mut target, dt);
                (o, [0.0; 2])
            } else {
                step_motion(view, max_offset(computed), next, velocity.v, &mut target, dt)
            };
            motion.wheel_target = target;
            next = o;
            if velocity.v != v {
                velocity.v = v;
            }
        }
        // Bring a newly focused descendant into view, on both axes.
        if focus_moved
            && let Some(focused) = focus_state.focused
            && focused != entity
            && crate::focus::in_scope(&parents, entity, focused)
            && let Ok(fc) = all_nodes.get(focused)
            && fc.rect[2] > 0.0
        {
            let shifted = focus_shift(computed.rect, fc.rect);
            if shifted != [0.0; 2] {
                next = clamp_offset([next[0] + shifted[0], next[1] + shifted[1]], viewport, content);
            }
        }
        if next != offset.0 {
            offset.0 = next;
            any_moved = true;
        }
        motion.last_offset = offset.0;
    }
    if any_moved {
        // Offsets only move positions and clip rects: layout re-walks without recomputing.
        scroll_dirty.mark();
    }
}

/// The scroll views around `start` in `world`, innermost first (see [`scroll_chain`]).
pub fn scroll_chain_in(world: &World, start: Entity) -> Vec<Entity> {
    scroll_chain(
        start,
        |e| world.get::<ScrollView>(e).is_some(),
        |e| world.get::<ChildOf>(e).map(|p| p.0),
    )
}

/// Scrolls the views around `start` by an offset-space `delta` (positive moves toward the end of
/// the content) the way a wheel over `start` would: innermost first, each on its own axes, the
/// rest chaining outward, never past an edge. Stops any fling or wheel easing on the chain and
/// returns each view with its new offset, innermost first. Used by the agent's `ui.scroll`.
pub fn scroll_by(world: &mut World, start: Entity, delta: [f32; 2]) -> Vec<(Entity, [f32; 2])> {
    let chain = scroll_chain_in(world, start);
    let links: Vec<ChainLink> = chain
        .iter()
        .filter_map(|&e| {
            let view = world.get::<ScrollView>(e)?;
            let computed = world.get::<ComputedNode>(e).copied().unwrap_or_default();
            let offset = world.get::<ScrollOffset>(e).map_or([0.0; 2], |o| o.0);
            Some(chain_link(&computed, offset, view))
        })
        .collect();
    if links.len() != chain.len() {
        return Vec::new();
    }
    let next = distribute(&links, delta, ScrollInput::Wheel);
    for (&entity, new) in chain.iter().zip(&next) {
        if let Some(mut offset) = world.get_mut::<ScrollOffset>(entity)
            && offset.0 != *new
        {
            offset.0 = *new;
        }
        if let Some(mut velocity) = world.get_mut::<ScrollVelocity>(entity) {
            velocity.v = [0.0; 2];
        }
        if let Some(mut motion) = world.get_mut::<ScrollMotion>(entity) {
            motion.cancel();
        }
    }
    if let Some(mut dirty) = world.get_resource_mut::<ScrollDirty>() {
        dirty.mark();
    }
    chain.into_iter().zip(next).collect()
}

/// Starts a fling of `velocity` (offset px per second) on a scroll view, limited to its axes and
/// to [`crate::scroll_motion::MAX_FLING_SPEED`]. Returns the velocity applied, or `None` when
/// `entity` is not a scroll view.
pub fn start_fling(world: &mut World, entity: Entity, velocity: [f32; 2]) -> Option<[f32; 2]> {
    let view = world.get::<ScrollView>(entity)?.clone();
    let max = crate::scroll_motion::MAX_FLING_SPEED;
    let mut v = [velocity[0].clamp(-max, max), velocity[1].clamp(-max, max)];
    for (axis, value) in v.iter_mut().enumerate() {
        if !view.axes.allows(axis) || view.inertia <= 0.0 {
            *value = 0.0;
        }
    }
    if let Some(mut current) = world.get_mut::<ScrollVelocity>(entity) {
        current.v = v;
    } else {
        world.entity_mut(entity).insert(ScrollVelocity { v });
    }
    if let Some(mut motion) = world.get_mut::<ScrollMotion>(entity) {
        motion.cancel();
        if v != [0.0; 2] {
            motion.note_fling();
        }
    }
    Some(v)
}

/// Stops a scroll view where it is: no fling, no wheel easing, and an overscrolled offset moves
/// back onto its edge. Returns `false` when `entity` is not a scroll view.
pub fn stop_scroll(world: &mut World, entity: Entity) -> bool {
    if world.get::<ScrollView>(entity).is_none() {
        return false;
    }
    let computed = world.get::<ComputedNode>(entity).copied().unwrap_or_default();
    let max = max_offset(&computed);
    if let Some(mut offset) = world.get_mut::<ScrollOffset>(entity) {
        let clamped = [offset.0[0].clamp(0.0, max[0]), offset.0[1].clamp(0.0, max[1])];
        if clamped != offset.0 {
            offset.0 = clamped;
        }
    }
    if let Some(mut velocity) = world.get_mut::<ScrollVelocity>(entity) {
        velocity.v = [0.0; 2];
    }
    if let Some(mut motion) = world.get_mut::<ScrollMotion>(entity) {
        motion.cancel();
    }
    if let Some(mut dirty) = world.get_resource_mut::<ScrollDirty>() {
        dirty.mark();
    }
    true
}

pub use crate::list::virtualise_lists;

/// Number of live rows of a list (for tests and diagnostics).
pub fn live_rows(list: &ListView) -> usize {
    list.pool.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_clamped_to_content() {
        // content 2000 - viewport 300 = max offset 1700; 1000 is already in range.
        let clamped = clamp_offset([-5.0, 1000.0], [400.0, 300.0], [800.0, 2000.0]);
        assert_eq!(clamped, [0.0, 1000.0]);
        let over = clamp_offset([0.0, 9999.0], [400.0, 300.0], [800.0, 2000.0]);
        assert_eq!(over, [0.0, 1700.0]);
        let small = clamp_offset([10.0, 10.0], [400.0, 300.0], [100.0, 100.0]);
        assert_eq!(small, [0.0, 0.0]);
    }

    #[test]
    fn focus_shift_brings_nodes_into_view() {
        let view = [0.0, 100.0, 200.0, 300.0];
        // Inside: no movement.
        assert_eq!(focus_shift(view, [10.0, 150.0, 50.0, 40.0]), [0.0, 0.0]);
        // Below the bottom edge (400): scroll down by the overhang.
        assert_eq!(focus_shift(view, [10.0, 380.0, 50.0, 40.0]), [0.0, 20.0]);
        // Above the top edge: scroll up.
        assert_eq!(focus_shift(view, [10.0, 80.0, 50.0, 40.0]), [0.0, -20.0]);
        // Right of the view: horizontal scroll.
        assert_eq!(focus_shift(view, [180.0, 150.0, 50.0, 40.0]), [30.0, 0.0]);
        // Taller than the view: its top lines up instead of its bottom.
        assert_eq!(focus_shift(view, [10.0, 120.0, 50.0, 500.0]), [0.0, 20.0]);
        assert_eq!(focus_shift(view, [10.0, 450.0, 50.0, 500.0]), [0.0, 350.0]);
    }

    #[test]
    fn inertia_decays_exponentially() {
        let v0 = [0.0, 100.0];
        let v1 = decay(v0, 0.1, 0.2);
        assert!(v1[1] < v0[1] && v1[1] > 0.0);
        // Analytic: 100 * e^{-0.5}
        assert!((v1[1] - 100.0 * (-0.5f32).exp()).abs() < 1e-3);
        let dead = decay(v0, 1.0, 0.0);
        assert_eq!(dead, [0.0, 0.0]);
    }

    /// Ported from the audit's `focus_scroll_delta` test to `focus_shift` (the two-axis helper
    /// that replaced it): the vertical shift is zero when visible, else the overhang.
    #[test]
    fn focus_scroll_delta_is_zero_when_visible() {
        let view = [0.0, 100.0, 400.0, 200.0];
        assert_eq!(focus_shift(view, [0.0, 120.0, 10.0, 20.0])[1], 0.0);
        assert_eq!(focus_shift(view, [0.0, 80.0, 10.0, 20.0])[1], -20.0);
        assert_eq!(focus_shift(view, [0.0, 290.0, 10.0, 30.0])[1], 20.0);
    }

    #[test]
    fn list_count_sources() {
        let list = ListView::default();
        assert_eq!(list.count(None, Some(&ItemCount(7))), 7);
        assert_eq!(
            list.count(
                Some(&ListItems(vec![ListItem::default(); 3])),
                Some(&ItemCount(7))
            ),
            3
        );
        assert_eq!(list.count(None, None), 0);
    }

    fn view(overscroll: Overscroll) -> ScrollView {
        ScrollView {
            inertia: 0.3,
            ..ScrollView::default()
        }
        .with_overscroll(overscroll)
    }

    #[test]
    fn a_clamped_fling_coasts_and_stops_at_the_edge() {
        let v = view(Overscroll::Clamp);
        let mut wheel = None;
        // 1000 px/s from 100 with plenty of room: covers v * tau = 300 px in the limit.
        let (mut o, mut vel) = ([0.0, 100.0], [0.0, 1000.0]);
        for _ in 0..200 {
            (o, vel) = step_motion(&v, [0.0, 5000.0], o, vel, &mut wheel, 1.0 / 60.0);
        }
        assert_eq!(vel, [0.0, 0.0]);
        assert!((o[1] - 400.0).abs() < 2.0, "{o:?}");
        // Near the end it stops at the edge, velocity gone.
        let (o, vel) = step_motion(&v, [0.0, 120.0], [0.0, 100.0], [0.0, 1000.0], &mut wheel, 0.1);
        assert_eq!((o[1], vel[1]), (120.0, 0.0));
    }

    #[test]
    fn the_spring_settles_without_crossing_the_edge() {
        let v = view(Overscroll::Bounce);
        let mut wheel = None;
        let max = [0.0, 100.0];
        let mut o = [0.0, 108.0];
        let mut vel = [0.0, -400.0];
        for _ in 0..120 {
            (o, vel) = step_motion(&v, max, o, vel, &mut wheel, 1.0 / 60.0);
            assert!(o[1] + 1e-3 >= 100.0, "the spring crossed into the content: {o:?}");
        }
        assert!((o[1] - 100.0).abs() < 0.5, "{o:?}");
        assert!(vel[1].abs() < 4.0, "{vel:?}");
    }

    /// Corr-ui-b: a tap on a view at rest set the spring pause, and only a later drag or wheel
    /// cleared it, so a list whose content shrank after the tap (a deleted row) never clamped and
    /// kept drawing the empty space past its new end.
    #[test]
    fn a_tap_on_a_resting_view_does_not_stop_a_later_clamp_or_spring() {
        use bevy_ecs::system::RunSystemOnce;
        let run = |overscroll: Overscroll| {
            let mut world = World::new();
            let mut time = regia_app::RealTime::default();
            time.advance_by(std::time::Duration::from_millis(16));
            world.insert_resource(time);
            world.init_resource::<UiPointer>();
            world.init_resource::<Messages<UiEvent>>();
            world.init_resource::<crate::layers::InputLayers>();
            world.init_resource::<crate::focus::FocusState>();
            world.init_resource::<ScrollDirty>();
            let view = world
                .spawn((
                    view(overscroll),
                    ScrollOffset([0.0, 500.0]),
                    ScrollVelocity::default(),
                    ScrollMotion::default(),
                    ComputedNode {
                        rect: [0.0, 0.0, 100.0, 100.0],
                        clip_rect: [0.0, 0.0, 100.0, 100.0],
                        content_size: [100.0, 1000.0],
                        scale: 1.0,
                        ..Default::default()
                    },
                ))
                .id();
            // A tap: pressed for one frame, released the next, no drag.
            world.resource_mut::<UiPointer>().pressed = Some(view);
            world.run_system_once(ui_scroll).expect("runs");
            world.resource_mut::<UiPointer>().pressed = None;
            world.run_system_once(ui_scroll).expect("runs");
            // The content shrinks under the view: its end is now at 200.
            world.get_mut::<ComputedNode>(view).expect("node").content_size = [100.0, 300.0];
            for _ in 0..240 {
                world.run_system_once(ui_scroll).expect("runs");
            }
            world.get::<ScrollOffset>(view).expect("offset").0
        };
        assert_eq!(
            run(Overscroll::Clamp),
            [0.0, 200.0],
            "a clamping view clamps to its new end"
        );
        let sprung = run(Overscroll::Bounce);
        assert!(
            (sprung[1] - 200.0).abs() < 0.5,
            "a bouncing view springs back: {sprung:?}"
        );
    }

    #[test]
    fn a_press_and_release_does_not_restart_the_spring() {
        let mut motion = ScrollMotion {
            spring_paused: true,
            ..ScrollMotion::default()
        };
        release_pause(&mut motion, [0.0; 2]);
        assert!(motion.spring_paused);
        assert!(
            !motion_integrates(&motion),
            "a release with no drag leaves the spring paused"
        );
        motion.held = true;
        assert!(!motion_integrates(&motion));
        motion.held = false;
        release_pause(&mut motion, [0.0, 10.0]);
        assert!(!motion.spring_paused);
        assert!(motion_integrates(&motion), "a real fling runs");
        motion.spring_paused = true;
        motion.wheel_target = Some([0.0, 1.0]);
        release_pause(&mut motion, [0.0; 2]);
        assert!(motion_integrates(&motion), "a wheel target runs");
    }

    #[test]
    fn a_bouncing_fling_overshoots_and_springs_back() {
        let v = view(Overscroll::Bounce);
        let mut wheel = None;
        let max = [0.0, 120.0];
        let (mut o, mut vel) = ([0.0, 100.0], [0.0, 2000.0]);
        let mut peak: f32 = 0.0;
        for _ in 0..240 {
            (o, vel) = step_motion(&v, max, o, vel, &mut wheel, 1.0 / 60.0);
            peak = peak.max(o[1]);
        }
        assert!(peak > 120.0, "the fling ran past the edge ({peak})");
        assert_eq!((o[1], vel[1]), (120.0, 0.0), "and came to rest on it");
    }

    #[test]
    fn with_reduced_motion_a_running_fling_and_a_spring_stop_at_once() {
        use regia_app::prelude::App;
        let run = |reduced: bool, offset: [f32; 2], velocity: [f32; 2]| {
            let mut app = App::new();
            app.add_plugins(crate::UiPlugin::code_only())
                .add_message::<regia_platform::WindowResized>()
                .add_message::<regia_platform::WindowScaleFactorChanged>()
                .add_message::<regia_platform::CursorMoved>()
                .add_message::<regia_platform::MouseButtonInput>()
                .add_message::<regia_platform::TouchInput>()
                .add_message::<regia_platform::MouseWheel>()
                .add_message::<regia_platform::KeyboardInput>()
                .add_message::<regia_platform::ImeEvent>();
            app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(16),
            ));
            app.insert_resource(crate::scale::UiAccessibility {
                reduced_motion: reduced,
                ..Default::default()
            });
            let view = app
                .world_mut()
                .spawn((
                    ScrollView {
                        overscroll: Overscroll::Bounce,
                        ..Default::default()
                    },
                    ScrollOffset(offset),
                    ScrollVelocity { v: velocity },
                    ComputedNode {
                        rect: [0.0, 0.0, 100.0, 100.0],
                        clip_rect: [0.0, 0.0, 100.0, 100.0],
                        content_size: [100.0, 1000.0],
                        scale: 1.0,
                        ..Default::default()
                    },
                ))
                .id();
            app.update();
            app.update();
            let offset = app
                .world()
                .get::<ScrollOffset>(view)
                .map(|o| o.0)
                .unwrap_or_default();
            let velocity = app
                .world()
                .get::<ScrollVelocity>(view)
                .map(|v| v.v)
                .unwrap_or_default();
            (offset, velocity)
        };
        // A fling: coasts with the setting off, stops where it is with it on.
        let (coasted, still_moving) = run(false, [0.0, 100.0], [0.0, 500.0]);
        assert!(coasted[1] > 100.0, "{coasted:?}");
        assert!(still_moving[1] > 0.0, "{still_moving:?}");
        let (stopped, velocity) = run(true, [0.0, 100.0], [0.0, 500.0]);
        assert_eq!(stopped, [0.0, 100.0]);
        assert_eq!(velocity, [0.0; 2]);
        // Past the edge: springs back over several frames, or lands on the edge at once.
        let (springing, _) = run(false, [0.0, -40.0], [0.0; 2]);
        assert!(springing[1] < 0.0, "{springing:?}");
        let (landed, _) = run(true, [0.0, -40.0], [0.0; 2]);
        assert_eq!(landed, [0.0, 0.0]);
    }

    #[test]
    fn wheel_smoothing_eases_to_its_target() {
        let v = ScrollView::default().with_wheel_smoothing(0.08);
        let mut wheel = Some([0.0, 300.0]);
        let (o1, _) = step_motion(&v, [0.0, 1000.0], [0.0, 0.0], [0.0; 2], &mut wheel, 1.0 / 60.0);
        assert!(o1[1] > 0.0 && o1[1] < 300.0, "{o1:?}");
        let mut o = o1;
        for _ in 0..120 {
            (o, _) = step_motion(&v, [0.0, 1000.0], o, [0.0; 2], &mut wheel, 1.0 / 60.0);
        }
        assert_eq!(o, [0.0, 300.0]);
        assert_eq!(wheel, None);
        // Without smoothing a target is reached at once.
        let mut jump = Some([0.0, 50.0]);
        let (o, _) = step_motion(
            &ScrollView::default(),
            [0.0, 1000.0],
            [0.0; 2],
            [0.0; 2],
            &mut jump,
            0.01,
        );
        assert_eq!((o, jump), ([0.0, 50.0], None));
    }

    #[test]
    fn a_locked_axis_never_moves() {
        let v = ScrollView::vertical();
        let mut wheel = None;
        let (o, vel) = step_motion(&v, [500.0, 500.0], [10.0, 10.0], [800.0, 800.0], &mut wheel, 0.05);
        assert_eq!((o[0], vel[0]), (10.0, 0.0));
        assert!(o[1] > 10.0);
    }

    #[test]
    fn chains_run_innermost_first() {
        // a inside b inside c; b is not a scroll view.
        let id = |i: u32| Entity::from_raw_u32(i).unwrap();
        let (a, b, c) = (id(1), id(2), id(3));
        let parent = |e: Entity| {
            if e == a {
                Some(b)
            } else if e == b {
                Some(c)
            } else {
                None
            }
        };
        assert_eq!(scroll_chain(a, |e| e != b, parent), vec![a, c]);
        assert_eq!(scroll_chain(b, |e| e != b, parent), vec![c]);
    }

    #[test]
    fn settled_means_no_motion_left() {
        let computed = ComputedNode {
            rect: [0.0, 0.0, 100.0, 100.0],
            content_size: [100.0, 400.0],
            ..Default::default()
        };
        let motion = ScrollMotion::default();
        let still = ScrollVelocity::default();
        assert!(is_settled(
            &computed,
            &ScrollOffset([0.0, 300.0]),
            &still,
            &motion
        ));
        assert!(!is_settled(
            &computed,
            &ScrollOffset([0.0, 320.0]),
            &still,
            &motion
        ));
        let moving = ScrollVelocity { v: [0.0, 50.0] };
        assert!(!is_settled(
            &computed,
            &ScrollOffset([0.0, 100.0]),
            &moving,
            &motion
        ));
        assert_eq!(max_offset(&computed), [0.0, 300.0]);
    }
}
