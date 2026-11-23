// SPDX-License-Identifier: MPL-2.0
//! Pointer and touch interaction (`specs/18` § 4): hit testing on [`ComputedNode`] with clip and
//! pick-through rules, widget states (`hovered`, `pressed`, `disabled`) and [`UiEvent`]s
//! (click, enter/leave, drag, value change, submit, focus change) as ECS messages.

use regia_app::prelude::*;

use crate::node::{ComputedNode, Disabled, Hovered, Interactable, Pressed, UiNode};

/// A value carried by `ValueChanged`.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum UiValue {
    /// No payload.
    #[default]
    None,
    /// Boolean (toggles).
    Bool(bool),
    /// Number (sliders, progress).
    Number(f32),
    /// Index into an option list (dropdown, tab bar).
    Index(usize),
    /// Text (text input submitted).
    Text(String),
}

/// Which pointer button produced a [`UiEvent`] (`Primary` for touch, `Submit` and everything that
/// is not a mouse button).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UiButton {
    /// Left mouse button, a finger, or a non-pointer activation.
    #[default]
    Primary,
    /// Right mouse button.
    Secondary,
    /// Middle mouse button.
    Middle,
}

impl UiButton {
    /// Parses `primary`, `secondary` or `middle` (agent protocol names).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "primary" | "left" => Some(Self::Primary),
            "secondary" | "right" => Some(Self::Secondary),
            "middle" => Some(Self::Middle),
            _ => None,
        }
    }

    /// The protocol name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Secondary => "secondary",
            Self::Middle => "middle",
        }
    }
}

/// Keyboard modifiers held when a [`UiEvent`] was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UiModifiers {
    /// Shift held.
    pub shift: bool,
    /// Control held.
    pub ctrl: bool,
    /// Alt (Option) held.
    pub alt: bool,
    /// Super (Windows, Command) held.
    pub meta: bool,
}

impl UiModifiers {
    /// Whether no modifier is held.
    pub fn is_empty(self) -> bool {
        !(self.shift || self.ctrl || self.alt || self.meta)
    }

    /// Updates the state from a key event; returns whether `logical_key` was a modifier key.
    pub fn apply_key(&mut self, logical_key: &str, down: bool) -> bool {
        let lower = logical_key.to_ascii_lowercase();
        let slot = match lower.as_str() {
            "shift" => &mut self.shift,
            "control" | "ctrl" => &mut self.ctrl,
            "alt" | "altgraph" | "option" => &mut self.alt,
            "super" | "meta" | "os" | "command" => &mut self.meta,
            _ => return false,
        };
        *slot = down;
        true
    }
}

/// UI events delivered as messages (`specs/18` § 4).
#[derive(Message, Debug, Clone, PartialEq)]
pub struct UiEvent {
    /// The node the event is about (the *sender*).
    pub target: Entity,
    /// What happened.
    pub kind: UiEventKind,
    /// The button that produced the event ([`UiButton::Primary`] when none applies).
    pub button: UiButton,
    /// Modifiers held at the time.
    pub modifiers: UiModifiers,
    /// Consecutive clicks counted by the double-click rules (1 for everything that is not a
    /// pointer click).
    pub click_count: u8,
}

impl UiEvent {
    /// An event with the primary button, no modifiers and a click count of 1.
    pub fn new(target: Entity, kind: UiEventKind) -> Self {
        Self {
            target,
            kind,
            button: UiButton::Primary,
            modifiers: UiModifiers::default(),
            click_count: 1,
        }
    }

    /// Sets the button.
    #[must_use]
    pub fn with_button(mut self, button: UiButton) -> Self {
        self.button = button;
        self
    }

    /// Sets the modifiers.
    #[must_use]
    pub fn with_modifiers(mut self, modifiers: UiModifiers) -> Self {
        self.modifiers = modifiers;
        self
    }

    /// Sets the click count.
    #[must_use]
    pub fn with_click_count(mut self, click_count: u8) -> Self {
        self.click_count = click_count;
        self
    }
}

/// Event kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEventKind {
    /// Activated (click released over the node, or `Submit` while focused).
    Click,
    /// Pointer entered the node.
    PointerEnter,
    /// Pointer left the node.
    PointerLeave,
    /// Dragging started on the node.
    DragStart,
    /// Dragging moved (delta since the previous event, physical px).
    Drag([f32; 2]),
    /// Dragging ended.
    DragEnd,
    /// A widget's value changed.
    ValueChanged(UiValue),
    /// Text submitted (Enter).
    Submitted(String),
    /// Focus moved to or from the node (`true` = gained).
    FocusChanged(bool),
    /// Second press-release within the double-click window on the same node. Sent in addition
    /// to the second [`UiEventKind::Click`] (which keeps its old meaning), with `click_count` 2.
    DoubleClick,
    /// Secondary button released over the node it was pressed on (context menu request).
    ContextClick,
    /// Middle button released over the node it was pressed on.
    MiddleClick,
    /// Wheel or trackpad scroll over the node (delta in physical px, as in `UiPointer::wheel`).
    Scroll([f32; 2]),
    /// A [`LongPressable`] node was held past its delay (sent once per press; the release then
    /// sends no `Click`).
    LongPress,
    /// A [`RepeatOnHold`] node is still held (carries the repeat number, starting at 1; the
    /// release then sends no `Click`).
    Repeat(u32),
}

/// Opt-in: the node sends [`UiEventKind::LongPress`] when held for `delay` seconds.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct LongPressable {
    /// Hold time in seconds of real time (unaffected by pause or time scale).
    pub delay: f32,
}

impl Default for LongPressable {
    fn default() -> Self {
        Self { delay: 0.5 }
    }
}

/// Opt-in: the node sends [`UiEventKind::Repeat`] while held (spin buttons, steppers).
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct RepeatOnHold {
    /// Hold time before the first repeat, seconds of real time (unaffected by pause or time scale).
    pub delay: f32,
    /// Time between repeats, seconds of real time (unaffected by pause or time scale).
    pub interval: f32,
}

impl Default for RepeatOnHold {
    fn default() -> Self {
        Self {
            delay: 0.4,
            interval: 0.1,
        }
    }
}

/// Timing rules for multi-click detection, in seconds of [`regia_app::RealTime`].
///
/// A manual frame step advances this clock by the step. Pause and `time_scale` change
/// [`regia_app::Time`] only, so they do not freeze or stretch the window.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct UiInputConfig {
    /// Largest gap between two clicks that still counts as a double click, seconds.
    pub double_click_window: f32,
    /// Largest distance between the two clicks, physical px.
    pub double_click_slop: f32,
}

impl Default for UiInputConfig {
    fn default() -> Self {
        Self {
            double_click_window: 0.4,
            double_click_slop: 8.0,
        }
    }
}

/// Counts consecutive clicks on the same node.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ClickTracker {
    last: Option<(Entity, f64, [f32; 2], u8)>,
}

impl ClickTracker {
    /// Registers a click on `target` at `time` (seconds) and `pos`; returns the click count:
    /// 2 when it follows a click on the same node within `window` seconds and `slop` px.
    pub fn register(&mut self, target: Entity, time: f64, pos: [f32; 2], window: f64, slop: f32) -> u8 {
        let count = match self.last {
            Some((e, t, p, n))
                if e == target
                    && time - t <= window
                    && (pos[0] - p[0]).hypot(pos[1] - p[1]) <= slop
                    && n < u8::MAX =>
            {
                n + 1
            }
            _ => 1,
        };
        self.last = Some((target, time, pos, count));
        count
    }
}

/// Hold timing for one press: long press once, repeats while held. Pure, so it is tested with
/// fixed steps.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HoldTimer {
    held_for: f64,
    long_fired: bool,
    repeats: u32,
    next_repeat: f64,
}

impl HoldTimer {
    /// Advances by `dt` seconds of real time (unaffected by pause or time scale) while the node stays pressed. Returns whether the
    /// long press fires now and the repeat number if a repeat fires now (at most one per step, so
    /// a long frame never bursts).
    pub fn step(
        &mut self,
        dt: f64,
        long_press: Option<f64>,
        repeat: Option<(f64, f64)>,
    ) -> (bool, Option<u32>) {
        self.held_for += dt;
        let mut long = false;
        if let Some(delay) = long_press
            && !self.long_fired
            && self.held_for >= delay
        {
            self.long_fired = true;
            long = true;
        }
        let mut rep = None;
        if let Some((delay, interval)) = repeat {
            let due = if self.repeats == 0 {
                delay
            } else {
                self.next_repeat
            };
            if self.held_for >= due {
                self.repeats += 1;
                self.next_repeat = self.held_for + interval;
                rep = Some(self.repeats);
            }
        }
        (long, rep)
    }

    /// Whether a long press or repeat fired during this press (the release then is not a click).
    pub fn fired(&self) -> bool {
        self.long_fired || self.repeats > 0
    }
}

/// Click and hold bookkeeping between frames.
#[derive(Resource, Debug, Default)]
pub struct UiClickState {
    now: f64,
    clicks: ClickTracker,
    hold: HoldTimer,
    /// Buttons down last frame (primary, secondary, middle): a press arms only on the rising edge.
    was_down: [bool; 3],
    /// The top input layer last frame.
    last_layer: Option<Entity>,
}

/// A hit box at least [`Self::min_px`] UI px on each side, centered on the node's rect (ADR-4903):
/// a control drawn smaller than a fingertip (a 44x24 toggle) still takes the touch the design
/// spec asks for (`size.touch-min`, WCAG 2.2 SC 2.5.8). Only hit testing and the accessibility
/// audit's target-size rule read it; the node lays out and draws as before. Space neighbors so
/// their boxes do not overlap: where two do, the one drawn later wins, as for any overlap.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct TouchTarget {
    /// The smallest hit box edge, in UI px (`UiDensity::touch_min`).
    pub min_px: f32,
}

impl TouchTarget {
    /// `rect` (x, y, w, h, physical px) grown to at least `min_px * scale` on each side, around
    /// its center. A non-finite or negative size grows nothing.
    pub fn grow(&self, rect: [f32; 4], scale: f32) -> [f32; 4] {
        let min = self.min_px * scale;
        if !min.is_finite() || min <= 0.0 {
            return rect;
        }
        let w = rect[2].max(min);
        let h = rect[3].max(min);
        [rect[0] - (w - rect[2]) * 0.5, rect[1] - (h - rect[3]) * 0.5, w, h]
    }
}

/// Raw pointer state fed from platform messages (physical px).
#[derive(Resource, Debug, Clone, Default)]
pub struct UiPointer {
    /// Cursor position (None when outside the window).
    pub cursor: Option<[f32; 2]>,
    /// Primary button down.
    pub primary_down: bool,
    /// The interactable under the pointer, if any.
    pub hovered: Option<Entity>,
    /// The node being pressed, if any.
    pub pressed: Option<Entity>,
    /// The node being dragged (set once movement exceeds a threshold while pressed).
    pub dragging: Option<Entity>,
    /// Previous pointer position (for drag deltas).
    pub last_pos: Option<[f32; 2]>,
    /// Positions of active touches by id.
    pub touches: std::collections::HashMap<u64, [f32; 2]>,
    /// The touch currently driving interaction (first finger).
    pub active_touch: Option<u64>,
    /// Whether the UI consumed pointer input this frame (gameplay input should not see it,
    /// `specs/18` § 4 "Input routing").
    pub consumed_pointer: bool,
    /// Cumulative scroll delta this frame (wheel / trackpad), physical px; consumed by the
    /// scroll system.
    pub wheel: [f32; 2],
    /// Secondary (right) button down.
    pub secondary_down: bool,
    /// Middle button down.
    pub middle_down: bool,
    /// The node the secondary button went down on.
    pub secondary_pressed: Option<Entity>,
    /// The node the middle button went down on.
    pub middle_pressed: Option<Entity>,
    /// Keyboard modifiers currently held.
    pub modifiers: UiModifiers,
}

impl UiPointer {
    /// The effective pointer position (an active touch drives when present).
    pub fn position(&self) -> Option<[f32; 2]> {
        if let Some(id) = self.active_touch {
            return self.touches.get(&id).copied();
        }
        self.cursor
    }

    /// Whether the primary button or a finger is down.
    pub fn down(&self) -> bool {
        self.primary_down || self.active_touch.is_some()
    }
}

/// Mutable access to a tracked button's down flag by slot: 0 left (primary), 1 right
/// (secondary), 2 middle. Side-button events are not tracked.
fn button_down(pointer: &mut UiPointer, slot: usize) -> &mut bool {
    match slot {
        0 => &mut pointer.primary_down,
        1 => &mut pointer.secondary_down,
        _ => &mut pointer.middle_down,
    }
}

/// Tracks raw pointer state from platform messages (`PreUpdate`).
pub fn update_pointer(
    mut pointer: ResMut<UiPointer>,
    mut cursor: MessageReader<regia_platform::CursorMoved>,
    mut buttons: MessageReader<regia_platform::MouseButtonInput>,
    mut touches: MessageReader<regia_platform::TouchInput>,
    mut wheel: MessageReader<regia_platform::MouseWheel>,
    mut mode: ResMut<crate::focus::UiInputMode>,
    mut keys: MessageReader<regia_platform::KeyboardInput>,
    mut lift_next_frame: Local<Option<(u64, [f32; 2])>>,
    // Left/right/middle releases deferred from last frame's batch (a whole click inside one
    // frame is kept down for that frame and lifted in the next).
    mut lift_buttons: Local<[bool; 3]>,
) {
    let _span = tracing::debug_span!("update_pointer").entered();
    // A finger that went down and up within the last frame was held down for that frame (see
    // below); it lifts now, before this frame's touches.
    if let Some((id, pos)) = lift_next_frame.take() {
        lift_touch(&mut pointer, id, pos, true);
    }
    // A mouse button whose press and release landed in one batch was kept down for that frame;
    // it lifts now, before this frame's button events.
    for i in 0..3 {
        if lift_buttons[i] {
            *button_down(&mut pointer, i) = false;
            lift_buttons[i] = false;
        }
    }
    for e in keys.read() {
        let down = e.state == regia_platform::ButtonState::Pressed;
        pointer.modifiers.apply_key(&e.logical_key, down);
    }
    for e in cursor.read() {
        pointer.cursor = Some([e.position.0 as f32, e.position.1 as f32]);
        *mode = crate::focus::UiInputMode::Pointer;
    }
    // A whole click inside one frame's batch (a low frame rate, or OS-level automation such as
    // `xdotool click`): plain last-wins batching would leave the button up, the interaction
    // pass would see neither the press nor the release edge, and the click would be lost. Keep
    // the button down for this frame and lift it in the next — the deferral touch taps get
    // (ADR-4932), extended to mouse buttons.
    let mut batch_down = [None::<bool>; 3];
    let mut batch_pressed = [false; 3];
    for e in buttons.read() {
        *mode = crate::focus::UiInputMode::Pointer;
        let down = e.state == regia_platform::ButtonState::Pressed;
        let idx = match e.button {
            regia_platform::MouseButton::Left => Some(0),
            regia_platform::MouseButton::Right => Some(1),
            regia_platform::MouseButton::Middle => Some(2),
            _ => None,
        };
        if let Some(i) = idx {
            if down {
                batch_pressed[i] = true;
            }
            batch_down[i] = Some(down);
        }
    }
    for i in 0..3 {
        let state = button_down(&mut pointer, i);
        match batch_down[i] {
            Some(true) => *state = true,
            Some(false) if batch_pressed[i] => {
                *state = true;
                lift_buttons[i] = true;
            }
            Some(false) => *state = false,
            None => {}
        }
    }
    // Fingers that went down in this frame's batch.
    let mut started_now: Vec<u64> = Vec::new();
    for e in touches.read() {
        let pos = [e.position.0 as f32, e.position.1 as f32];
        match e.phase {
            regia_platform::TouchPhase::Started => {
                if lift_next_frame.is_some_and(|(id, _)| id == e.id) {
                    // The same finger id came down again in this batch: the new touch wins.
                    *lift_next_frame = None;
                }
                // A finger already down (a repeated start) is not a new press this frame.
                if !pointer.touches.contains_key(&e.id) {
                    started_now.push(e.id);
                }
                pointer.touches.insert(e.id, pos);
                if pointer.active_touch.is_none() {
                    pointer.active_touch = Some(e.id);
                }
            }
            regia_platform::TouchPhase::Moved => {
                pointer.touches.insert(e.id, pos);
            }
            // A whole tap inside one frame (a low frame rate, or `adb shell input tap`): the
            // interaction pass would never see the finger down, so the tap would click nothing
            // (ADR-4932). Keep it down at its lift point for this frame and lift it in the next.
            regia_platform::TouchPhase::Ended
                if started_now.contains(&e.id)
                    && pointer.active_touch == Some(e.id)
                    && lift_next_frame.is_none() =>
            {
                pointer.touches.insert(e.id, pos);
                *lift_next_frame = Some((e.id, pos));
            }
            regia_platform::TouchPhase::Ended | regia_platform::TouchPhase::Cancelled => {
                lift_touch(
                    &mut pointer,
                    e.id,
                    pos,
                    e.phase == regia_platform::TouchPhase::Ended,
                );
            }
        }
    }
    for e in wheel.read() {
        let scale = match e.unit {
            regia_platform::WheelUnit::Line => 40.0,
            regia_platform::WheelUnit::Pixel => 1.0,
        };
        pointer.wheel[0] += e.delta.0 * scale;
        pointer.wheel[1] += e.delta.1 * scale;
    }
}

/// Removes touch `id`. When it was the driving finger, the pointer stays where it lifted (`ended`),
/// as a mouse stays where it clicked: the release is hit-tested there, so a tap clicks. Without
/// this the release had no position and a tap never clicked. A cancelled touch does not click.
fn lift_touch(pointer: &mut UiPointer, id: u64, pos: [f32; 2], ended: bool) {
    pointer.touches.remove(&id);
    if pointer.active_touch == Some(id) {
        pointer.active_touch = None;
        pointer.last_pos = None;
        if ended {
            pointer.cursor = Some(pos);
        }
    }
}

/// One hit-test candidate.
#[derive(Debug, Clone, Copy)]
pub struct HitCandidate {
    /// The node.
    pub entity: Entity,
    /// Rect (x, y, w, h).
    pub rect: [f32; 4],
    /// Clip rect.
    pub clip: [f32; 4],
    /// Skip for hit testing.
    pub pick_through: bool,
    /// Disabled nodes are not hit.
    pub disabled: bool,
    /// Depth: higher draws later (wins ties).
    pub z: f32,
    /// Tree preorder index: breaks ties between equal `z` (higher draws later).
    pub paint_order: u32,
}

/// Picks the topmost candidate containing `p`: highest z wins; ties go to the last candidate
/// (later in tree order). Clipped-away nodes are ignored.
pub fn pick_topmost(candidates: impl Iterator<Item = HitCandidate>, p: [f32; 2]) -> Option<Entity> {
    let mut best: Option<((f32, u32), Entity)> = None;
    for c in candidates {
        if c.pick_through || c.disabled || c.clip[2] <= 0.0 || c.clip[3] <= 0.0 {
            continue;
        }
        let inside = |r: [f32; 4]| p[0] >= r[0] && p[1] >= r[1] && p[0] <= r[0] + r[2] && p[1] <= r[1] + r[3];
        if !inside(c.rect) || !inside(c.clip) {
            continue;
        }
        if best.is_none_or(|(k, _)| c.z > k.0 || (c.z == k.0 && c.paint_order >= k.1)) {
            best = Some(((c.z, c.paint_order), c.entity));
        }
    }
    best.map(|(_, e)| e)
}

/// A node's hit rects under the window pointer (ADR-2406): unchanged for window UI, offset to the
/// viewport for a window camera's root, `None` for camera-texture and world-panel nodes (they
/// are clicked through `ui.world_click`).
#[must_use]
pub fn window_rects(
    rect: [f32; 4],
    clip: [f32; 4],
    tag: Option<&crate::world_target::UiTargetNode>,
) -> Option<([f32; 4], [f32; 4])> {
    match tag {
        None => Some((rect, clip)),
        Some(tag) => {
            let [x, y] = tag.window_origin?;
            Some((
                [rect[0] + x, rect[1] + y, rect[2], rect[3]],
                [clip[0] + x, clip[1] + y, clip[2], clip[3]],
            ))
        }
    }
}

/// A window point in the space of a node's `ComputedNode` rects: [`window_rects`]'s offset undone,
/// for what is stored there (the masks of [`crate::group_layer::UiClipMasks`]).
#[must_use]
pub fn node_space(p: [f32; 2], tag: Option<&crate::world_target::UiTargetNode>) -> [f32; 2] {
    match tag.and_then(|t| t.window_origin) {
        Some([x, y]) => [p[0] - x, p[1] - y],
        None => p,
    }
}

/// Hit test helper over a query.
pub fn hit_test(
    nodes: &Query<(Entity, &ComputedNode, &Interactable, Option<&Disabled>)>,
    p: [f32; 2],
) -> Option<Entity> {
    let candidates = nodes
        .iter()
        .map(|(entity, computed, interactable, disabled)| HitCandidate {
            entity,
            rect: computed.rect,
            clip: computed.clip_rect,
            pick_through: interactable.pick_through,
            disabled: disabled.is_some(),
            z: computed.z,
            paint_order: computed.paint_order,
        });
    pick_topmost(candidates, p)
}

/// Whether the UI consumed pointer input this frame (gameplay gating).
#[derive(Resource, Debug, Default)]
pub struct UiInputState {
    /// Set while any UI node consumes pointer input.
    pub pointer_consumed: bool,
    /// The input mode of the top [`crate::layers::InputLayer`] (gameplay reads this to know
    /// whether it should act; `UiAndGame` when no layer is active).
    pub layer_mode: crate::layers::LayerInputMode,
}

/// `Update`: hover/press state machine, clicks, drags, secondary and middle clicks, scrolling,
/// double clicks, long presses and repeats. Only the top [`crate::layers::InputLayer`]'s subtree
/// is hit-testable while a layer is active.
pub fn ui_interaction(
    mut pointer: ResMut<UiPointer>,
    mut events: MessageWriter<UiEvent>,
    nodes: HitNodes,
    _added: Query<Entity, (Added<Interactable>, With<UiNode>)>,
    mut commands: Commands,
    layers: Res<crate::layers::InputLayers>,
    parents: Query<&ChildOf>,
    disabled_subtrees: Query<(), With<crate::node::DisabledSubtree>>,
    holds: Query<(Option<&LongPressable>, Option<&RepeatOnHold>)>,
    time: Res<regia_app::RealTime>,
    config: Res<UiInputConfig>,
    mut clicks: ResMut<UiClickState>,
    renderer: Option<Res<crate::render::SharedUiRenderer>>,
    input_state: Option<ResMut<UiInputState>>,
) {
    let _span = tracing::trace_span!("ui_interaction").entered();
    let dt = time.delta().as_secs_f64();
    clicks.now += dt;
    let now = clicks.now;
    let pos = pointer.position();
    let top_layer = layers.top_root();
    let game_only = layers.mode() == crate::layers::LayerInputMode::GameOnly;
    let hit = pos.filter(|_| !game_only).and_then(|p| {
        pointer_hit(
            &nodes,
            &parents,
            &disabled_subtrees,
            top_layer,
            renderer.as_deref(),
            p,
        )
    });
    let down = pointer.down() && !game_only;
    let modifiers = pointer.modifiers;

    // A press that began outside the new top layer ends when the layer stack changes under it.
    if clicks.last_layer != top_layer {
        clicks.last_layer = top_layer;
        if let Some(root) = top_layer
            && let Some(p) = pointer.pressed
            && !crate::focus::in_scope(&parents, root, p)
        {
            if pointer.dragging == Some(p) {
                events.write(UiEvent::new(p, UiEventKind::DragEnd).with_modifiers(modifiers));
            }
            if let Ok(mut cmds) = commands.get_entity(p) {
                cmds.remove::<Pressed>();
            }
            pointer.pressed = None;
            pointer.dragging = None;
            pointer.last_pos = None;
        }
    }
    let was_down = clicks.was_down;
    clicks.was_down = [
        down,
        pointer.secondary_down && !game_only,
        pointer.middle_down && !game_only,
    ];

    let disabled = |entity| {
        crate::node::subtree_disabled(entity, &parents, &disabled_subtrees)
            || nodes
                .get(entity)
                .is_ok_and(|(_, _, _, local, ..)| local.is_some())
    };
    if let Some(pressed) = pointer.pressed
        && disabled(pressed)
    {
        if let Ok(mut entity) = commands.get_entity(pressed) {
            entity.remove::<Pressed>();
        }
        pointer.pressed = None;
        pointer.dragging = None;
        pointer.last_pos = None;
    }
    if pointer.secondary_pressed.is_some_and(disabled) {
        pointer.secondary_pressed = None;
    }
    if pointer.middle_pressed.is_some_and(disabled) {
        pointer.middle_pressed = None;
    }
    let prev_hovered = pointer.hovered;
    let prev_pressed = pointer.pressed;
    let prev_dragging = pointer.dragging;

    // Hover transitions.
    if prev_hovered != hit {
        if let Some(old) = prev_hovered {
            events.write(UiEvent::new(old, UiEventKind::PointerLeave).with_modifiers(modifiers));
            if let Ok(mut cmds) = commands.get_entity(old) {
                cmds.remove::<Hovered>();
            }
        }
        if let Some(new) = hit {
            events.write(UiEvent::new(new, UiEventKind::PointerEnter).with_modifiers(modifiers));
            commands.entity(new).insert(Hovered);
        }
        pointer.hovered = hit;
    }

    // Press transitions.
    if down && !was_down[0] && hit.is_some() && prev_pressed.is_none() {
        pointer.pressed = hit;
        pointer.dragging = None;
        pointer.last_pos = pos;
        clicks.hold = HoldTimer::default();
        if let Some(p) = hit {
            commands.entity(p).insert(Pressed);
        }
    }
    if !down && prev_pressed.is_some() {
        let released = prev_pressed;
        let fired_hold = clicks.hold.fired();
        if hit == released
            && !fired_hold
            && let Some(target) = released
        {
            let at = pos.unwrap_or([0.0, 0.0]);
            let count = clicks.clicks.register(
                target,
                now,
                at,
                f64::from(config.double_click_window),
                config.double_click_slop,
            );
            events.write(
                UiEvent::new(target, UiEventKind::Click)
                    .with_modifiers(modifiers)
                    .with_click_count(count),
            );
            if count == 2 {
                events.write(
                    UiEvent::new(target, UiEventKind::DoubleClick)
                        .with_modifiers(modifiers)
                        .with_click_count(2),
                );
            }
        }
        if let Some(p) = released {
            if let Ok(mut cmds) = commands.get_entity(p) {
                cmds.remove::<Pressed>();
            }
            if prev_dragging == Some(p) {
                events.write(UiEvent::new(p, UiEventKind::DragEnd).with_modifiers(modifiers));
            }
        }
        pointer.pressed = None;
        pointer.dragging = None;
        pointer.last_pos = None;
    }

    // Long press and repeat-on-hold for opted-in nodes still under the pointer.
    if let Some(pressed_entity) = pointer.pressed
        && prev_pressed == Some(pressed_entity)
        && hit == Some(pressed_entity)
        && pointer.dragging.is_none()
        && let Ok((long, repeat)) = holds.get(pressed_entity)
        && (long.is_some() || repeat.is_some())
    {
        let (long_now, repeat_now) = clicks.hold.step(
            dt,
            long.map(|l| f64::from(l.delay)),
            repeat.map(|r| (f64::from(r.delay), f64::from(r.interval))),
        );
        if long_now {
            events.write(UiEvent::new(pressed_entity, UiEventKind::LongPress).with_modifiers(modifiers));
        }
        if let Some(n) = repeat_now {
            events.write(UiEvent::new(pressed_entity, UiEventKind::Repeat(n)).with_modifiers(modifiers));
        }
    }

    // Secondary and middle buttons: press remembers the node, release over it is the click.
    for (is_down, was_down, button, kind) in [
        (
            pointer.secondary_down,
            was_down[1],
            UiButton::Secondary,
            UiEventKind::ContextClick,
        ),
        (
            pointer.middle_down,
            was_down[2],
            UiButton::Middle,
            UiEventKind::MiddleClick,
        ),
    ] {
        let slot = if button == UiButton::Secondary {
            &mut pointer.secondary_pressed
        } else {
            &mut pointer.middle_pressed
        };
        let is_down = is_down && !game_only;
        if is_down && !was_down && slot.is_none() {
            *slot = hit;
        } else if !is_down
            && let Some(target) = slot.take()
            && hit == Some(target)
        {
            events.write(
                UiEvent::new(target, kind)
                    .with_button(button)
                    .with_modifiers(modifiers),
            );
        }
    }

    // Drag detection: travel is measured from the press point until it exceeds the threshold.
    // A slow drag (under 2 px per frame) still starts once the total leaves the press point.
    if let (Some(pressed_entity), Some(p)) = (pointer.pressed, pointer.position()) {
        // Copy the flag first: `last_pos` is borrowed mutably on the same call.
        let dragging = pointer.dragging.is_some();
        let (started, moved) = consider_drag(&mut pointer.last_pos, dragging, p);
        if started {
            pointer.dragging = Some(pressed_entity);
            events.write(UiEvent::new(pressed_entity, UiEventKind::DragStart).with_modifiers(modifiers));
        }
        if let Some(delta) = moved {
            events.write(UiEvent::new(pressed_entity, UiEventKind::Drag(delta)).with_modifiers(modifiers));
        }
    }

    // Wheel over a node: a Scroll event (the scroll system still reads the same delta).
    if pointer.wheel != [0.0, 0.0]
        && let Some(target) = hit
    {
        events.write(UiEvent::new(target, UiEventKind::Scroll(pointer.wheel)).with_modifiers(modifiers));
    }

    pointer.consumed_pointer = pointer.hovered.is_some() || pointer.pressed.is_some();
    // The gameplay-facing mirror (`specs/18` § 4 input routing); it was documented but never written.
    if let Some(mut state) = input_state {
        state.pointer_consumed = pointer.consumed_pointer;
    }
}

/// The node query hit testing reads ([`ui_interaction`] and [`claim_pointer_buttons`]).
pub type HitNodes<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static ComputedNode,
        &'static Interactable,
        Option<&'static Disabled>,
        Option<&'static crate::tween::UiLayoutBox>,
        Option<&'static crate::world_target::UiTargetNode>,
        Option<&'static crate::group_layer::UiClipMasks>,
        Option<&'static TouchTarget>,
    ),
>;

/// The interactable under window point `p`: the top layer's subtree only (when a layer is active),
/// transformed nodes where they draw, camera UI offset to its viewport, rounded clips and image
/// masks honored. Shared by [`ui_interaction`] and [`claim_pointer_buttons`] so a press the UI
/// takes is exactly a press the claim covers.
fn pointer_hit(
    nodes: &HitNodes,
    parents: &Query<&ChildOf>,
    disabled_subtrees: &Query<(), With<crate::node::DisabledSubtree>>,
    top_layer: Option<Entity>,
    renderer: Option<&crate::render::SharedUiRenderer>,
    p: [f32; 2],
) -> Option<Entity> {
    // One lock copies the few alpha maps this frame's image masks need. The candidate loop then
    // looks them up without holding the renderer mutex (a missing map stays opaque, as before).
    let alpha_maps = {
        let mut ids = std::collections::HashSet::new();
        if renderer.is_some() {
            for (_, _, _, _, _, _, masks, _) in nodes.iter() {
                if let Some(masks) = masks {
                    for shape in &masks.0 {
                        if let crate::group_layer::MaskShape::Image { image, .. } = *shape {
                            ids.insert(image);
                        }
                    }
                }
            }
        }
        if ids.is_empty() {
            std::collections::HashMap::new()
        } else if let Some(renderer) = renderer.as_ref() {
            let guard = renderer.lock();
            ids.into_iter()
                .filter_map(|id| guard.copy_mask_alpha(id).map(|map| (id, map)))
                .collect::<std::collections::HashMap<_, _>>()
        } else {
            std::collections::HashMap::new()
        }
    };
    let candidates = nodes
        .iter()
        .filter(|(entity, ..)| top_layer.is_none_or(|root| crate::focus::in_scope(parents, root, *entity)))
        .filter_map(
            |(entity, computed, interactable, disabled, layout_box, tag, masks, target)| {
                // A transformed node is hit where it is drawn, unless it asks for its layout rect.
                let (rect, clip) = crate::tween::hit_rects(computed, layout_box);
                let rect = target.map_or(rect, |t| t.grow(rect, computed.scale));
                // Camera and world UI (ADR-2406) is offset to its viewport or not hit at all.
                let (rect, clip) = window_rects(rect, clip, tag)?;
                // Rounded clips and image masks (ADR-2866): a point they hide does not hit.
                if let Some(masks) = masks
                    && !masks.admits(node_space(p, tag), |id, uv| {
                        alpha_maps.get(&id).map(|map| map.sample(uv))
                    })
                {
                    return None;
                }
                Some(HitCandidate {
                    entity,
                    rect,
                    clip,
                    pick_through: interactable.pick_through,
                    disabled: disabled.is_some()
                        || crate::node::subtree_disabled(entity, parents, disabled_subtrees),
                    z: computed.z,
                    paint_order: computed.paint_order,
                })
            },
        );
    pick_topmost(candidates, p)
}

/// `PreUpdate`, after [`update_pointer`] and [`regia_input::InputSet::Devices`], before
/// [`regia_input::InputSet::Evaluation`]: the UI half of the press-to-release mouse latch
/// (ADR-7976, `specs/18` § 4). A mouse button whose press edge is this frame is claimed while the
/// pointer is over a hit-testable node (the same hit [`ui_interaction`] takes) or while a press
/// already belongs to a node; a claimed button is released when it is up again. Action evaluation
/// reads claimed buttons as up, so a click on a widget never reaches gameplay. Runs before
/// evaluation in the same frame: a claim written later would let the press edge through once.
/// Hit testing runs only on frames with a press edge.
pub fn claim_pointer_buttons(
    pointer: Res<UiPointer>,
    mouse: Option<Res<regia_input::Mouse>>,
    claims: Option<ResMut<regia_input::PointerClaims>>,
    nodes: HitNodes,
    layers: Res<crate::layers::InputLayers>,
    parents: Query<&ChildOf>,
    disabled_subtrees: Query<(), With<crate::node::DisabledSubtree>>,
    renderer: Option<Res<crate::render::SharedUiRenderer>>,
) {
    let (Some(mouse), Some(mut claims)) = (mouse, claims) else {
        return;
    };
    if !claims.enabled()
        || (claims.claimed().is_empty() && !regia_input::PointerClaims::has_press_edge(&mouse))
    {
        return;
    }
    let _span = tracing::trace_span!("claim_pointer_buttons").entered();
    let over_ui = regia_input::PointerClaims::has_press_edge(&mouse)
        && layers.mode() != crate::layers::LayerInputMode::GameOnly
        && (pointer.pressed.is_some()
            || pointer.secondary_pressed.is_some()
            || pointer.middle_pressed.is_some()
            || pointer.position().is_some_and(|p| {
                pointer_hit(
                    &nodes,
                    &parents,
                    &disabled_subtrees,
                    layers.top_root(),
                    renderer.as_deref(),
                    p,
                )
                .is_some()
            }));
    claims.track(&mouse, over_ui);
}

/// Manhattan travel, in px, from the press point before a drag starts.
const DRAG_THRESHOLD: f32 = 2.0;

/// Whether `p` starts or continues a drag.
///
/// `last` stays at the press point until travel from that point exceeds [`DRAG_THRESHOLD`].
/// Once `dragging` is set, any nonzero move is a drag and `last` advances to `p`.
/// Returns `(started, delta)`.
fn consider_drag(last: &mut Option<[f32; 2]>, dragging: bool, p: [f32; 2]) -> (bool, Option<[f32; 2]>) {
    let Some(origin) = *last else {
        *last = Some(p);
        return (false, None);
    };
    let delta = [p[0] - origin[0], p[1] - origin[1]];
    let travel = delta[0].abs() + delta[1].abs();
    if dragging || travel > DRAG_THRESHOLD {
        let started = !dragging && travel > DRAG_THRESHOLD;
        let moved = if travel > 0.0 { Some(delta) } else { None };
        *last = Some(p);
        (started, moved)
    } else {
        (false, None)
    }
}

/// A [`UiEvent`] re-delivered to an ancestor of its target: the bubble phase (`specs/18` § 4,
/// ADR-7070). The event itself keeps its meaning on its target; ancestors (a panel, a list, the
/// screen root) read these copies to handle their descendants' events in one place, and scripts on
/// them receive the copy as `ScriptEvent::Ui` (`regia_framework`).
///
/// [`bubble_ui_events`] writes one copy per ancestor, nearest first, for every event but
/// `PointerEnter`/`PointerLeave` (hover is about one node). The walk stops after a node that
/// carries [`UiStopBubble`].
#[derive(Message, Debug, Clone, PartialEq)]
pub struct UiBubbledEvent {
    /// The ancestor this copy is delivered to (the handler).
    pub current: Entity,
    /// The event as it was sent; `event.target` is the node it is about.
    pub event: UiEvent,
    /// Steps from the target: 1 for its parent.
    pub depth: u32,
}

/// Stops the bubble phase at this node: its own handlers see its descendants' events, its
/// ancestors do not (a modal that keeps its clicks to itself).
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UiStopBubble;

/// Whether events of this kind bubble to ancestors (all but hover enter and leave).
pub fn bubbles(kind: &UiEventKind) -> bool {
    !matches!(kind, UiEventKind::PointerEnter | UiEventKind::PointerLeave)
}

/// The ancestors an event on `target` bubbles to, nearest first: up the `ChildOf` chain, stopping
/// after a node with [`UiStopBubble`] (a target that carries it bubbles nowhere), and after at most
/// [`crate::focus::MAX_ANCESTOR_STEPS`] links so a corrupt cycle ends.
pub fn bubble_path(
    parents: &Query<&ChildOf>,
    stops: &Query<(), With<UiStopBubble>>,
    target: Entity,
    out: &mut Vec<Entity>,
) {
    out.clear();
    if stops.contains(target) {
        return;
    }
    let mut current = target;
    for _ in 0..crate::focus::MAX_ANCESTOR_STEPS {
        let Ok(parent) = parents.get(current) else {
            return;
        };
        current = parent.parent();
        out.push(current);
        if stops.contains(current) {
            return;
        }
    }
}

/// `Update` (after the interaction, navigation and widget passes): writes a [`UiBubbledEvent`] for
/// each ancestor of every bubbling [`UiEvent`] (`specs/18` § 4, ADR-7070). Events written later in
/// the frame bubble on the next pass; nothing is lost because the reader keeps its cursor.
pub fn bubble_ui_events(
    mut events: MessageReader<UiEvent>,
    mut out: MessageWriter<UiBubbledEvent>,
    parents: Query<&ChildOf>,
    stops: Query<(), With<UiStopBubble>>,
    mut path: Local<Vec<Entity>>,
) {
    let _span = tracing::trace_span!("ui_bubble").entered();
    for event in events.read() {
        if !bubbles(&event.kind) {
            continue;
        }
        bubble_path(&parents, &stops, event.target, &mut path);
        for (depth, &ancestor) in (1u32..).zip(path.iter()) {
            out.write(UiBubbledEvent {
                current: ancestor,
                event: event.clone(),
                depth,
            });
        }
    }
}

/// `Update` (after [`ui_interaction`]): hands the pointer-consumed flag to the input layer's
/// [`regia_input::UiPointerGate`], so gameplay contexts do not see a click the UI took
/// (`specs/18` § 4 input routing, ADR-7070). Writes only on a change.
///
/// While [`regia_input::PointerClaims`] is enabled (the default with `UiPlugin`), the gate follows
/// press ownership only: it is set while a primary press belongs to a node (`UiPointer::pressed`),
/// not while the pointer merely hovers one. Claims already hide a widget press from its press
/// frame; a hover gate would also hide a world press that moves over UI and, being a frame late,
/// the first world press after the pointer leaves a widget, both of which ADR-7976 gives to the
/// game (ADR-8786). With claims disabled the gate keeps ADR-7070's hover-or-press reading.
pub fn sync_pointer_gate(
    pointer: Res<UiPointer>,
    claims: Option<Res<regia_input::PointerClaims>>,
    gate: Option<ResMut<regia_input::UiPointerGate>>,
) {
    let consumed = if claims.is_some_and(|claims| claims.enabled()) {
        pointer.pressed.is_some()
    } else {
        pointer.consumed_pointer
    };
    if let Some(mut gate) = gate
        && gate.ui_consumed != consumed
    {
        gate.ui_consumed = consumed;
    }
}

/// Delivers `kind` on `target` and re-emits it on each ancestor **as a [`UiEvent`] targeting the
/// ancestor** (a synthetic event). The automatic bubble phase is [`bubble_ui_events`], which keeps
/// the original target in a [`UiBubbledEvent`] instead; use this only to fake an event on a chain.
pub fn bubble_ui_event(
    events: &mut MessageWriter<UiEvent>,
    target: Entity,
    ancestors: impl IntoIterator<Item = Entity>,
    kind: UiEventKind,
) {
    events.write(UiEvent::new(target, kind.clone()));
    for ancestor in ancestors {
        events.write(UiEvent::new(ancestor, kind.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use regia_ecs::ecs::system::RunSystemOnce;

    #[test]
    fn slow_drag_accumulates_past_the_press_point() {
        let mut last = Some([0.0, 0.0]);
        let (started, moved) = consider_drag(&mut last, false, [1.5, 0.0]);
        assert!(!started);
        assert!(moved.is_none());
        assert_eq!(
            last,
            Some([0.0, 0.0]),
            "a sub-threshold frame does not move the origin"
        );
        let (started, moved) = consider_drag(&mut last, false, [3.0, 0.0]);
        assert!(started, "3 px from the press point starts the drag");
        assert_eq!(moved, Some([3.0, 0.0]));
        assert_eq!(last, Some([3.0, 0.0]));
        // After the drag has started, a frame under the threshold still moves.
        let (started, moved) = consider_drag(&mut last, true, [4.0, 0.0]);
        assert!(!started);
        assert_eq!(moved, Some([1.0, 0.0]));
    }

    fn candidate(e: Entity, rect: [f32; 4], z: f32) -> HitCandidate {
        HitCandidate {
            entity: e,
            rect,
            clip: [f32::MIN / 4.0, f32::MIN / 4.0, f32::MAX, f32::MAX],
            pick_through: false,
            disabled: false,
            z,
            paint_order: 0,
        }
    }

    #[test]
    fn picks_topmost_z() {
        let a = candidate(Entity::from_bits(1), [0.0, 0.0, 100.0, 100.0], 1.0);
        let b = candidate(Entity::from_bits(2), [0.0, 0.0, 100.0, 100.0], 2.0);
        assert_eq!(
            pick_topmost([a, b].into_iter(), [50.0, 50.0]),
            Some(Entity::from_bits(2))
        );
        assert_eq!(pick_topmost([a, b].into_iter(), [150.0, 50.0]), None);
    }

    #[test]
    fn equal_z_ties_go_to_the_later_paint_order_regardless_of_candidate_order() {
        let mut under = candidate(Entity::from_bits(1), [0.0, 0.0, 100.0, 100.0], 1.0);
        let mut over = candidate(Entity::from_bits(2), [0.0, 0.0, 100.0, 100.0], 1.0);
        under.paint_order = 3;
        over.paint_order = 7;
        for list in [[under, over], [over, under]] {
            assert_eq!(pick_topmost(list.into_iter(), [50.0, 50.0]), Some(over.entity));
        }
    }

    #[test]
    fn pick_through_and_disabled_skipped() {
        let mut a = candidate(Entity::from_bits(1), [0.0, 0.0, 100.0, 100.0], 2.0);
        a.pick_through = true;
        let b = candidate(Entity::from_bits(2), [0.0, 0.0, 100.0, 100.0], 1.0);
        assert_eq!(
            pick_topmost([a, b].into_iter(), [50.0, 50.0]),
            Some(Entity::from_bits(2))
        );
        let mut c = candidate(Entity::from_bits(3), [0.0, 0.0, 10.0, 10.0], 5.0);
        c.disabled = true;
        assert_eq!(
            pick_topmost([c, b].into_iter(), [5.0, 5.0]),
            Some(Entity::from_bits(2))
        );
    }

    #[test]
    fn clipped_nodes_ignored() {
        let mut a = candidate(Entity::from_bits(1), [0.0, 0.0, 100.0, 100.0], 2.0);
        a.clip = [0.0, 0.0, 0.0, 0.0];
        let b = candidate(Entity::from_bits(2), [0.0, 0.0, 100.0, 100.0], 1.0);
        assert_eq!(
            pick_topmost([a, b].into_iter(), [50.0, 50.0]),
            Some(Entity::from_bits(2))
        );
    }

    #[test]
    fn pointer_position_prefers_touch() {
        let mut p = UiPointer::default();
        p.cursor = Some([5.0, 5.0]);
        assert_eq!(p.position(), Some([5.0, 5.0]));
        p.touches.insert(1, [9.0, 9.0]);
        p.active_touch = Some(1);
        assert_eq!(p.position(), Some([9.0, 9.0]));
    }

    #[test]
    fn a_touch_target_grows_the_hit_box_around_its_center_and_never_shrinks_it() {
        let t = TouchTarget { min_px: 44.0 };
        assert_eq!(t.grow([100.0, 50.0, 44.0, 24.0], 1.0), [100.0, 40.0, 44.0, 44.0]);
        // Physical px at scale 2: 88 px.
        assert_eq!(t.grow([0.0, 0.0, 40.0, 100.0], 2.0), [-24.0, 0.0, 88.0, 100.0]);
        assert_eq!(t.grow([5.0, 5.0, 60.0, 60.0], 1.0), [5.0, 5.0, 60.0, 60.0]);
        let none = TouchTarget { min_px: f32::NAN };
        assert_eq!(none.grow([1.0, 2.0, 3.0, 4.0], 1.0), [1.0, 2.0, 3.0, 4.0]);
    }

    /// One frame of `update_pointer` with `touches` sent.
    fn touch_frame(world: &mut World, touches: &[(regia_platform::TouchPhase, [f64; 2])]) {
        for (phase, at) in touches {
            world.write_message(regia_platform::TouchInput {
                window: Entity::PLACEHOLDER,
                id: 3,
                phase: *phase,
                position: (at[0], at[1]),
                timestamp: Default::default(),
            });
        }
        world
            .run_system_once(update_pointer)
            .expect("update_pointer runs");
        world
            .resource_mut::<Messages<regia_platform::TouchInput>>()
            .update();
    }

    #[test]
    fn a_lifted_finger_leaves_the_pointer_where_it_lifted_and_a_cancelled_one_does_not() {
        use regia_platform::TouchPhase;
        let mut world = World::new();
        world.init_resource::<UiPointer>();
        world.init_resource::<crate::focus::UiInputMode>();
        world.init_resource::<Messages<regia_platform::CursorMoved>>();
        world.init_resource::<Messages<regia_platform::MouseButtonInput>>();
        world.init_resource::<Messages<regia_platform::TouchInput>>();
        world.init_resource::<Messages<regia_platform::MouseWheel>>();
        world.init_resource::<Messages<regia_platform::KeyboardInput>>();

        touch_frame(&mut world, &[(TouchPhase::Started, [40.0, 30.0])]);
        let p = world.resource::<UiPointer>();
        assert!(p.down());
        assert_eq!(p.position(), Some([40.0, 30.0]));
        // The lift frame: no finger down, and the release is hit-tested at the lift point.
        touch_frame(&mut world, &[(TouchPhase::Ended, [42.0, 31.0])]);
        let p = world.resource::<UiPointer>();
        assert!(!p.down());
        assert_eq!(p.position(), Some([42.0, 31.0]));

        world.resource_mut::<UiPointer>().cursor = None;
        touch_frame(&mut world, &[(TouchPhase::Started, [10.0, 10.0])]);
        touch_frame(&mut world, &[(TouchPhase::Cancelled, [10.0, 10.0])]);
        let p = world.resource::<UiPointer>();
        assert!(!p.down());
        assert_eq!(p.position(), None, "a cancelled touch has nowhere to click");
    }

    /// From the TEST Pixel 9 Pro (2026-11-09, ADR-4932): at a low frame rate a whole tap (finger
    /// down and up) arrives in one frame, the interaction pass never saw the finger down, and the
    /// tap clicked nothing. Such a finger is held down for that frame and lifts in the next one,
    /// which is the two-frame tap that clicks.
    #[test]
    fn a_tap_inside_one_frame_is_down_for_that_frame_and_lifts_in_the_next() {
        use regia_platform::TouchPhase;
        let mut world = World::new();
        world.init_resource::<UiPointer>();
        world.init_resource::<crate::focus::UiInputMode>();
        world.init_resource::<Messages<regia_platform::CursorMoved>>();
        world.init_resource::<Messages<regia_platform::MouseButtonInput>>();
        world.init_resource::<Messages<regia_platform::TouchInput>>();
        world.init_resource::<Messages<regia_platform::MouseWheel>>();
        world.init_resource::<Messages<regia_platform::KeyboardInput>>();
        // One system for every frame, so its state carries over as in an app.
        let mut schedule = bevy_ecs::schedule::Schedule::default();
        schedule.add_systems(update_pointer);
        let mut frame = |world: &mut World, touches: &[(u64, TouchPhase, [f64; 2])]| {
            for (id, phase, at) in touches {
                world.write_message(regia_platform::TouchInput {
                    window: Entity::PLACEHOLDER,
                    id: *id,
                    phase: *phase,
                    position: (at[0], at[1]),
                    timestamp: Default::default(),
                });
            }
            schedule.run(world);
            world
                .resource_mut::<Messages<regia_platform::TouchInput>>()
                .update();
        };

        frame(
            &mut world,
            &[
                (3, TouchPhase::Started, [40.0, 30.0]),
                (3, TouchPhase::Ended, [40.0, 30.0]),
            ],
        );
        let p = world.resource::<UiPointer>();
        assert!(p.down(), "the interaction pass must see the finger down");
        assert_eq!(p.position(), Some([40.0, 30.0]));
        frame(&mut world, &[]);
        let p = world.resource::<UiPointer>();
        assert!(!p.down());
        assert_eq!(
            p.position(),
            Some([40.0, 30.0]),
            "the release is hit-tested at the lift"
        );
        frame(&mut world, &[]);
        assert!(!world.resource::<UiPointer>().down());

        // A touch cancelled within its frame never clicks: it is gone at once.
        world.resource_mut::<UiPointer>().cursor = None;
        frame(
            &mut world,
            &[
                (4, TouchPhase::Started, [9.0, 9.0]),
                (4, TouchPhase::Cancelled, [9.0, 9.0]),
            ],
        );
        let p = world.resource::<UiPointer>();
        assert!(!p.down());
        assert_eq!(p.position(), None);

        // A finger that was already down lifts in its own frame, as before.
        frame(&mut world, &[(5, TouchPhase::Started, [1.0, 2.0])]);
        frame(&mut world, &[(5, TouchPhase::Ended, [3.0, 4.0])]);
        let p = world.resource::<UiPointer>();
        assert!(!p.down());
        assert_eq!(p.position(), Some([3.0, 4.0]));
    }

    #[test]
    fn clicks_count_within_window_and_slop_only() {
        let e = Entity::from_bits(1);
        let other = Entity::from_bits(2);
        let mut t = ClickTracker::default();
        assert_eq!(t.register(e, 0.0, [10.0, 10.0], 0.4, 8.0), 1);
        assert_eq!(t.register(e, 0.3, [12.0, 11.0], 0.4, 8.0), 2, "inside the window");
        assert_eq!(
            t.register(e, 0.5, [12.0, 11.0], 0.4, 8.0),
            3,
            "window runs from the last click"
        );
        assert_eq!(
            t.register(e, 1.0, [12.0, 11.0], 0.4, 8.0),
            1,
            "outside the window"
        );
        assert_eq!(t.register(e, 1.1, [100.0, 11.0], 0.4, 8.0), 1, "too far away");
        assert_eq!(t.register(other, 1.2, [100.0, 11.0], 0.4, 8.0), 1, "another node");
    }

    #[test]
    fn long_press_fires_once() {
        let mut h = HoldTimer::default();
        let mut fired = 0;
        for _ in 0..40 {
            let (long, rep) = h.step(0.05, Some(0.5), None);
            fired += usize::from(long);
            assert_eq!(rep, None);
        }
        assert_eq!(fired, 1);
        assert!(h.fired());
    }

    #[test]
    fn repeats_follow_delay_then_interval_and_never_burst() {
        let mut h = HoldTimer::default();
        let repeat = Some((0.5, 0.25));
        let mut at = Vec::new();
        for i in 1..=20 {
            if let (_, Some(n)) = h.step(0.125, None, repeat) {
                at.push((i, n));
            }
        }
        // Held 0.125 s per step: first repeat at 0.5 s (step 4), then every 0.25 s (2 steps).
        assert_eq!(
            at,
            vec![
                (4, 1),
                (6, 2),
                (8, 3),
                (10, 4),
                (12, 5),
                (14, 6),
                (16, 7),
                (18, 8),
                (20, 9)
            ]
        );
        let mut h = HoldTimer::default();
        assert_eq!(
            h.step(10.0, None, repeat),
            (false, Some(1)),
            "one repeat for a 10 s hitch"
        );
        assert_eq!(h.step(0.01, None, repeat), (false, None));
    }

    #[test]
    fn modifier_keys_track_down_and_up() {
        let mut m = UiModifiers::default();
        assert!(m.is_empty());
        assert!(m.apply_key("Shift", true));
        assert!(m.apply_key("Control", true));
        assert!(m.shift && m.ctrl && !m.alt);
        assert!(m.apply_key("Shift", false));
        assert!(!m.shift && m.ctrl);
        assert!(!m.apply_key("a", true), "not a modifier");
    }

    #[test]
    fn new_events_default_to_primary_without_modifiers() {
        let e = UiEvent::new(Entity::from_bits(1), UiEventKind::Click);
        assert_eq!((e.button, e.click_count), (UiButton::Primary, 1));
        assert!(e.modifiers.is_empty());
        assert_eq!(UiButton::parse("right"), Some(UiButton::Secondary));
        assert_eq!(UiButton::parse("x"), None);
    }

    /// Spawns one interactable. `Option` is not a bundle in this ECS, so a mask is inserted after.
    fn spawn_hit(world: &mut World, z: f32, masks: Option<crate::group_layer::UiClipMasks>) -> Entity {
        let id = world
            .spawn((
                UiNode,
                Interactable::default(),
                ComputedNode {
                    rect: [0.0, 0.0, 80.0, 80.0],
                    clip_rect: [f32::MIN / 4.0, f32::MIN / 4.0, f32::MAX, f32::MAX],
                    z,
                    scale: 1.0,
                    ..ComputedNode::default()
                },
            ))
            .id();
        if let Some(masks) = masks {
            world.entity_mut(id).insert(masks);
        }
        id
    }

    /// A `ChildOf` cycle under an active layer must not spin hit testing, and the node inside the
    /// layer is still the one hit.
    #[test]
    fn child_of_cycle_does_not_stall_hit_testing() {
        let mut world = World::new();
        world.init_resource::<crate::focus::FocusState>();
        world.init_resource::<crate::layers::InputLayers>();
        world.init_resource::<Messages<UiEvent>>();
        world.init_resource::<UiInputState>();
        world.init_resource::<UiPointer>();
        world.init_resource::<UiInputConfig>();
        world.init_resource::<UiClickState>();
        world.init_resource::<regia_app::RealTime>();
        let root = world.spawn(UiNode).id();
        crate::layers::push_layer(&mut world, root, crate::layers::LayerInputMode::UiOnly).expect("layer");
        let good = spawn_hit(&mut world, 1.0, None);
        world.entity_mut(root).add_child(good);
        let cycled = spawn_hit(&mut world, 50.0, None);
        let other = world.spawn(UiNode).id();
        world.entity_mut(cycled).add_child(other);
        world.entity_mut(other).add_child(cycled);
        world.resource_mut::<UiPointer>().cursor = Some([10.0, 10.0]);
        world.run_system_once(ui_interaction).expect("hit test returns");
        assert_eq!(world.resource::<UiPointer>().hovered, Some(good));
    }

    /// Several image masks share one renderer lock for the whole hit test.
    #[test]
    fn masked_hit_test_locks_the_renderer_once() {
        let mut world = World::new();
        world.init_resource::<Messages<UiEvent>>();
        world.init_resource::<UiInputState>();
        world.init_resource::<UiPointer>();
        world.init_resource::<UiInputConfig>();
        world.init_resource::<UiClickState>();
        world.init_resource::<regia_app::RealTime>();
        world.init_resource::<crate::layers::InputLayers>();
        world.insert_resource(crate::render::SharedUiRenderer(
            crate::render::UiRenderer::new_handle(),
        ));
        let mask = crate::group_layer::UiClipMasks(vec![crate::group_layer::MaskShape::Image {
            image: 7,
            rect: [0.0, 0.0, 100.0, 100.0],
            placed: [0.0, 0.0, 100.0, 100.0],
            uv: [0.0, 0.0, 1.0, 1.0],
            tile: false,
        }]);
        for z in [1.0, 2.0, 3.0] {
            spawn_hit(&mut world, z, Some(mask.clone()));
        }
        world.resource_mut::<UiPointer>().cursor = Some([10.0, 10.0]);
        let before = world
            .resource::<crate::render::SharedUiRenderer>()
            .lock()
            .lock_acquires();
        world.run_system_once(ui_interaction).expect("hit test");
        let after = world
            .resource::<crate::render::SharedUiRenderer>()
            .lock()
            .lock_acquires();
        // `before` already includes its own lock. Interaction adds one, and the trailing read adds one.
        assert_eq!(
            after - before,
            2,
            "one interaction lock plus the trailing read ({before} -> {after})"
        );
    }
}
