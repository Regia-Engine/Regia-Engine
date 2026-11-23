// SPDX-License-Identifier: MPL-2.0
//! Focus navigation for keyboard, gamepad and TV (`specs/18` § 4): directional movement by
//! geometry (nearest node in the direction with overlap preference, following the CSS spatial
//! navigation heuristic), explicit [`NavLinks`] overrides, focus scopes (modals trap focus),
//! default focus per screen, and the visible focus ring (rendered by extract).

use regia_app::prelude::*;

use std::time::Duration;

use regia_platform::{ButtonState, GamepadAxis, GamepadAxisChanged, GamepadButton, GamepadButtonChanged};

use crate::interaction::{UiEvent, UiEventKind};
use crate::node::{ComputedNode, Disabled, Focused};

/// Explicit focus-movement overrides.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NavLinks {
    /// Focus target when navigating up.
    pub up: Option<Entity>,
    /// Focus target when navigating down.
    pub down: Option<Entity>,
    /// Focus target when navigating left.
    pub left: Option<Entity>,
    /// Focus target when navigating right.
    pub right: Option<Entity>,
}

/// Marks a node as focusable.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Focusable {
    /// Explicit navigation overrides.
    pub nav: NavLinks,
}

/// Opts a [`crate::widgets::Slider`] out of the generic left/right value stepping of focus navigation
/// (a screen that steps its slider itself, with its own increments, adds this).
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ManualValueNav;

/// Focus scope: while any [`crate::widgets::Modal`] with an open scope exists, focus stays inside
/// the scope's subtree.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FocusScope;

/// The node receiving focus when a screen appears.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DefaultFocus;

/// Nav direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavDirection {
    /// Up.
    Up,
    /// Down.
    Down,
    /// Left.
    Left,
    /// Right.
    Right,
    /// Forward in document order (Tab).
    Next,
    /// Backward in document order.
    Prev,
}

impl NavDirection {
    /// The unit vector of a directional move (y down).
    pub fn vec(self) -> [f32; 2] {
        match self {
            Self::Up => [0.0, -1.0],
            Self::Down => [0.0, 1.0],
            Self::Left => [-1.0, 0.0],
            Self::Right => [1.0, 0.0],
            Self::Next | Self::Prev => [0.0, 0.0],
        }
    }
}

/// The current focus state.
#[derive(Resource, Debug, Default)]
pub struct FocusState {
    /// The focused node.
    pub focused: Option<Entity>,
    /// The active focus scope (deepest open scope containing the focus).
    pub scope: Option<Entity>,
    /// The node focused when no explicit focus exists (default focus candidate).
    pub default_focus: Option<Entity>,
}

/// External navigation actions (gamepad bindings from the input lane write these; the keyboard
/// path fills them from `KeyboardInput` messages).
#[derive(Resource, Debug, Default, Clone, Copy)]
pub struct UiNavActions {
    /// Up pressed this frame.
    pub up: bool,
    /// Down pressed this frame.
    pub down: bool,
    /// Left pressed this frame.
    pub left: bool,
    /// Right pressed this frame.
    pub right: bool,
    /// Next (Tab) pressed.
    pub next: bool,
    /// Previous (Shift-Tab) pressed.
    pub prev: bool,
    /// Submit (Enter / gamepad A).
    pub submit: bool,
    /// Cancel (Escape / gamepad B).
    pub cancel: bool,
    /// Value-increase (for sliders: right arrow / dpad right on focused widget).
    pub value_up: bool,
    /// Value-decrease.
    pub value_down: bool,
}

impl UiNavActions {
    /// Clears the one-frame flags.
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// What drove the UI last: decides whether the focus ring and controller prompts show. A
/// controller-first UI never requires the pointer: every interaction is reachable through
/// [`UiNavActions`] (which Steam Input's gamepad or keyboard-mouse templates all feed).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UiInputMode {
    /// Mouse, touch or trackpad (the default).
    #[default]
    Pointer,
    /// Gamepad or keyboard navigation: draw the focus ring and button prompts.
    Navigation,
}

/// When a focused node draws its focus ring (gap I05, `docs/specs/ui-default-design.md` § 5).
/// Focus itself never depends on this: only the ring does.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FocusRingPolicy {
    /// Only in [`UiInputMode::Navigation`] (the default). After a pointer move, click or touch
    /// the focused node keeps its focus but shows no ring until the next keyboard or gamepad
    /// input.
    #[default]
    NavigationOnly,
    /// Whenever a node is focused, in every input mode (an accessibility setting).
    Always,
    /// Never: the game draws its own focus visuals.
    Never,
}

impl FocusRingPolicy {
    /// Whether a focused node shows its ring in `mode`.
    pub fn shows_ring(self, mode: UiInputMode) -> bool {
        match self {
            Self::NavigationOnly => mode == UiInputMode::Navigation,
            Self::Always => true,
            Self::Never => false,
        }
    }

    /// The agent-protocol name: `navigation_only`, `always` or `never`.
    pub fn name(self) -> &'static str {
        match self {
            Self::NavigationOnly => "navigation_only",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

/// Ring width (UI px) for a focused node whose theme sets no `border.width.focus`.
pub const DEFAULT_FOCUS_RING_WIDTH: f32 = 2.0;

/// Where a focus ring draws, in the units of the border box it was computed from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FocusRingGeometry {
    /// Outer edge of the ring (x, y, w, h).
    pub rect: [f32; 4],
    /// Corner radius of the outer edge; the inner edge is this minus [`Self::width`].
    pub radius: f32,
    /// Band width.
    pub width: f32,
}

/// The focus ring around a widget's border box `border_box` (x, y, w, h) with corner radius
/// `radius`: a band `width` wide whose inner edge sits `offset` outside the border box. A
/// negative offset draws the ring inside the box (list rows use `-width`, so the band lies just
/// inside the row and a scroll clip cannot cut it). The inner edge follows the widget's radius
/// plus the offset, so the ring stays concentric with the widget; the outer radius is clamped to
/// half the ring's short side. `None` when the width is not positive or the ring has no area.
pub fn focus_ring_geometry(
    border_box: [f32; 4],
    radius: f32,
    width: f32,
    offset: f32,
) -> Option<FocusRingGeometry> {
    if width <= 0.0 || !width.is_finite() || !offset.is_finite() {
        return None;
    }
    let grow = offset + width;
    let rect = [
        border_box[0] - grow,
        border_box[1] - grow,
        border_box[2] + 2.0 * grow,
        border_box[3] + 2.0 * grow,
    ];
    // Also refuses a NaN rect, so the clamp below never sees a NaN bound.
    let half = rect[2].min(rect[3]) * 0.5;
    if half.is_nan() || half <= 0.0 {
        return None;
    }
    Some(FocusRingGeometry {
        rect,
        radius: (radius.max(0.0) + grow).clamp(0.0, half),
        width: width.min(half),
    })
}

/// Stick-to-navigation tuning (`ui_gamepad_navigation`).
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct GamepadNavConfig {
    /// Stick deflection that counts as a direction press.
    pub deadzone: f32,
    /// Hold time before a held direction starts repeating.
    pub repeat_delay: Duration,
    /// Time between repeats once repeating.
    pub repeat_interval: Duration,
}

impl Default for GamepadNavConfig {
    fn default() -> Self {
        Self {
            deadzone: 0.55,
            repeat_delay: Duration::from_millis(350),
            repeat_interval: Duration::from_millis(120),
        }
    }
}

/// Auto-repeat for one held direction (stick or d-pad), driven by frame time so it behaves the
/// same at 30 and 60 fps.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DirectionRepeat {
    held: Option<NavDirection>,
    /// Time the current direction has been held.
    held_for: Duration,
    /// Time since the last emitted press.
    since_emit: Duration,
    repeating: bool,
}

impl DirectionRepeat {
    /// Advances by `dt` with the direction currently held (`None` = released). Returns the
    /// direction to press this frame: at once on a new hold, again after
    /// [`GamepadNavConfig::repeat_delay`], then every [`GamepadNavConfig::repeat_interval`].
    /// A frame longer than the interval emits one press, not a burst.
    pub fn step(
        &mut self,
        now: Option<NavDirection>,
        dt: Duration,
        cfg: &GamepadNavConfig,
    ) -> Option<NavDirection> {
        if now != self.held {
            *self = Self {
                held: now,
                ..Self::default()
            };
            return now;
        }
        let dir = now?;
        self.held_for += dt;
        self.since_emit += dt;
        let due = if self.repeating {
            self.since_emit >= cfg.repeat_interval
        } else {
            self.held_for >= cfg.repeat_delay
        };
        if due {
            self.repeating = true;
            self.since_emit = Duration::ZERO;
            Some(dir)
        } else {
            None
        }
    }
}

/// The direction a stick deflection `(x right, y up)` means, `None` inside the deadzone. The
/// dominant axis wins so a diagonal push does not zig-zag.
pub fn stick_direction(x: f32, y: f32, deadzone: f32) -> Option<NavDirection> {
    if !x.is_finite() || !y.is_finite() || x.abs().max(y.abs()) < deadzone {
        return None;
    }
    Some(if x.abs() > y.abs() {
        if x > 0.0 {
            NavDirection::Right
        } else {
            NavDirection::Left
        }
    } else if y > 0.0 {
        NavDirection::Up
    } else {
        NavDirection::Down
    })
}

/// The `Menu` input context path of UI navigation (`specs/18` § 4, ADR-7070): whether it drives
/// navigation this frame, and the repeat of its held direction.
#[derive(Resource, Debug, Default)]
pub struct MenuNavState {
    active: bool,
    repeat: DirectionRepeat,
}

impl MenuNavState {
    /// Whether navigation read the `Menu` context on the last pass (the raw keyboard and gamepad
    /// paths were off).
    pub fn active(&self) -> bool {
        self.active
    }
}

/// Whether UI navigation reads the `Menu` input context: the config declares
/// [`regia_input::MENU_CONTEXT`] and player 0 has it enabled.
pub fn menu_context_active(
    config: Option<&regia_input::InputConfig>,
    contexts: Option<&regia_input::InputContexts>,
) -> bool {
    config.is_some_and(|c| c.contexts.contains_key(regia_input::MENU_CONTEXT))
        && contexts.is_some_and(|c| c.is_enabled(0, regia_input::MENU_CONTEXT))
}

/// `Update` (before [`ui_gamepad_navigation`]): while the `Menu` input context is declared and
/// enabled for player 0 ([`menu_context_active`]), fills [`UiNavActions`] from its actions, and the
/// raw keyboard and gamepad paths stand down, so rebinding the context rebinds navigation
/// (`specs/18` § 4, ADR-7070). Held directions (`Up`/`Down`/`Left`/`Right`, or the `Navigate`
/// stick's dominant direction) repeat like the gamepad path; `Next` with Shift held is `Prev`.
/// While a text input has focus, left/right and `Submit` (and up/down in a multi-line input) belong
/// to the editor, whatever device pressed them.
pub fn ui_menu_navigation(
    input: (
        Option<Res<regia_input::InputConfig>>,
        Option<Res<regia_input::InputContexts>>,
        Option<Res<regia_input::Actions>>,
    ),
    cfg: Res<GamepadNavConfig>,
    real: Res<regia_app::RealTime>,
    mut state: ResMut<MenuNavState>,
    mut nav: ResMut<UiNavActions>,
    focus: Res<FocusState>,
    pointer: Res<crate::interaction::UiPointer>,
    text_inputs: Query<&crate::text_input::TextInput>,
) {
    let _span = tracing::trace_span!("ui_menu_nav").entered();
    let (config, contexts, actions) = input;
    let active = menu_context_active(config.as_deref(), contexts.as_deref());
    if state.active != active {
        state.active = active;
        state.repeat = DirectionRepeat::default();
    }
    let Some(actions) = actions.filter(|_| active) else {
        return;
    };
    let editing = focus.focused.and_then(|f| text_inputs.get(f).ok());
    let owns_vertical = editing.is_some_and(|t| t.multiline);
    let owns_horizontal = editing.is_some();
    let pressed = |name: &str| actions.pressed(name);
    let held = if pressed(regia_input::MENU_UP) && !owns_vertical {
        Some(NavDirection::Up)
    } else if pressed(regia_input::MENU_DOWN) && !owns_vertical {
        Some(NavDirection::Down)
    } else if pressed(regia_input::MENU_LEFT) && !owns_horizontal {
        Some(NavDirection::Left)
    } else if pressed(regia_input::MENU_RIGHT) && !owns_horizontal {
        Some(NavDirection::Right)
    } else {
        let stick = actions.axis2d(regia_input::MENU_NAVIGATE);
        stick_direction(stick.x, stick.y, cfg.deadzone)
    };
    match state.repeat.step(held, real.delta(), &cfg) {
        Some(NavDirection::Up) => nav.up = true,
        Some(NavDirection::Down) => nav.down = true,
        Some(NavDirection::Left) => nav.left = true,
        Some(NavDirection::Right) => nav.right = true,
        _ => {}
    }
    let edge = |name: &str| actions.just_pressed_this_frame(name);
    if edge(regia_input::MENU_NEXT) {
        if pointer.modifiers.shift {
            nav.prev = true;
        } else {
            nav.next = true;
        }
    }
    nav.prev |= edge(regia_input::MENU_PREV);
    nav.submit |= edge(regia_input::MENU_SUBMIT) && !owns_horizontal;
    nav.cancel |= edge(regia_input::MENU_CANCEL);
}

/// Gamepad state the navigation system tracks between frames.
#[derive(Resource, Debug, Default)]
pub struct GamepadNavState {
    stick: [f32; 2],
    dpad: [bool; 4], // up, down, left, right
    repeat: DirectionRepeat,
}

/// `Update` (before [`ui_focus_navigation`]): translates the standard gamepad layout into
/// [`UiNavActions`]. D-pad and left stick move focus (with repeat while held), South (A) submits,
/// East (B) cancels, the shoulders step through the focus order, and while a slider is focused
/// left/right move its value instead (the same bindings Steam Input's gamepad template delivers).
pub fn ui_gamepad_navigation(
    mut buttons: MessageReader<GamepadButtonChanged>,
    mut axes: MessageReader<GamepadAxisChanged>,
    cfg: Res<GamepadNavConfig>,
    real: Res<regia_app::RealTime>,
    mut gp: ResMut<GamepadNavState>,
    mut actions: ResMut<UiNavActions>,
    mut mode: ResMut<UiInputMode>,
    layers: Res<crate::layers::InputLayers>,
    menu: Option<Res<MenuNavState>>,
) {
    let _span = tracing::trace_span!("ui_gamepad_nav").entered();
    if menu.is_some_and(|m| m.active()) {
        // The `Menu` input context drives navigation (ADR-7070): drop the raw pad messages and the
        // held state, so nothing fires when the context is turned off again.
        buttons.clear();
        axes.clear();
        *gp = GamepadNavState::default();
        return;
    }
    let mut used = false;
    for e in buttons.read() {
        let down = e.state == ButtonState::Pressed;
        match e.button {
            GamepadButton::DPadUp => gp.dpad[0] = down,
            GamepadButton::DPadDown => gp.dpad[1] = down,
            GamepadButton::DPadLeft => gp.dpad[2] = down,
            GamepadButton::DPadRight => gp.dpad[3] = down,
            GamepadButton::South if down => actions.submit = true,
            GamepadButton::East if down => actions.cancel = true,
            GamepadButton::LeftShoulder if down => actions.prev = true,
            GamepadButton::RightShoulder if down => actions.next = true,
            _ => continue,
        }
        used = true;
    }
    for e in axes.read() {
        match e.axis {
            GamepadAxis::LeftStickX => gp.stick[0] = e.value,
            GamepadAxis::LeftStickY => gp.stick[1] = e.value,
            // Some pads (and Steam Input virtual pads) report the d-pad as a hat axis.
            GamepadAxis::DPadX => {
                gp.dpad[2] = e.value < -0.5;
                gp.dpad[3] = e.value > 0.5;
            }
            GamepadAxis::DPadY => {
                gp.dpad[0] = e.value > 0.5;
                gp.dpad[1] = e.value < -0.5;
            }
            _ => continue,
        }
        used |= e.value.abs() >= cfg.deadzone;
    }
    let held = if gp.dpad[0] {
        Some(NavDirection::Up)
    } else if gp.dpad[1] {
        Some(NavDirection::Down)
    } else if gp.dpad[2] {
        Some(NavDirection::Left)
    } else if gp.dpad[3] {
        Some(NavDirection::Right)
    } else {
        stick_direction(gp.stick[0], gp.stick[1], cfg.deadzone)
    };
    let dt = real.delta();
    let pressed = gp.repeat.step(held, dt, &cfg);
    match pressed {
        Some(NavDirection::Up) => actions.up = true,
        Some(NavDirection::Down) => actions.down = true,
        Some(NavDirection::Left) => actions.left = true,
        Some(NavDirection::Right) => actions.right = true,
        _ => {}
    }
    // A top layer that hands input to the game leaves the focus ring and button prompts alone.
    let game_only = layers.mode() == crate::layers::LayerInputMode::GameOnly;
    if (used || pressed.is_some()) && !game_only {
        *mode = UiInputMode::Navigation;
    }
}

/// Scores how well `candidate` continues a move in `dir` from the `from` rect. Lower is better.
/// Projection distance along the direction dominates; lateral distance counts double when the
/// candidate does not overlap the source's extent perpendicular to the direction (overlap
/// preference). Following the CSS Spatial Navigation heuristic.
pub fn nav_score(from: [f32; 4], candidate: [f32; 4], dir: NavDirection) -> Option<f32> {
    let v = dir.vec();
    if v == [0.0, 0.0] {
        return None;
    }
    let (fx, fy, fw, fh) = (from[0], from[1], from[2], from[3]);
    let (cx, cy, cw, ch) = (candidate[0], candidate[1], candidate[2], candidate[3]);
    // Edge-to-edge primary distance in the move direction.
    let (primary, lateral, overlap) = if v[1] != 0.0 {
        // Vertical move.
        let (f_near, c_near) = if v[1] > 0.0 { (fy + fh, cy) } else { (fy, cy + ch) };
        let primary = (c_near - f_near) * v[1];
        let overlap = fx < cx + cw && cx < fx + fw;
        let lateral = if overlap {
            0.0
        } else {
            let f_mid = fx + fw / 2.0;
            let c_mid = cx + cw / 2.0;
            (c_mid - f_mid).abs() - (fw + cw) / 2.0
        };
        (primary, lateral, overlap)
    } else {
        // Horizontal move.
        let (f_near, c_near) = if v[0] > 0.0 { (fx + fw, cx) } else { (fx, cx + cw) };
        let primary = (c_near - f_near) * v[0];
        let overlap = fy < cy + ch && cy < fy + fh;
        let lateral = if overlap {
            0.0
        } else {
            let f_mid = fy + fh / 2.0;
            let c_mid = cy + ch / 2.0;
            (c_mid - f_mid).abs() - (fh + ch) / 2.0
        };
        (primary, lateral, overlap)
    };
    if primary < -0.001 {
        return None; // behind the source
    }
    Some(primary + lateral.max(0.0) * 2.0 + if overlap { 0.0 } else { 10.0 })
}

/// Whether a focus scope is active: a scope on a [`crate::widgets::Modal`] is active only while
/// the modal is open (`modal_open`), any other scope always is.
fn scope_is_open(modal_open: Option<bool>) -> bool {
    modal_open.unwrap_or(true)
}

/// The scope holding focus (`holds_focus`), else the last open scope.
fn pick_scope(open: &[Entity], holds_focus: impl Fn(Entity) -> bool) -> Option<Entity> {
    open.iter()
        .copied()
        .find(|s| holds_focus(*s))
        .or_else(|| open.last().copied())
}

/// `Update`: focus navigation from keyboard messages and [`UiNavActions`] (the input lane writes
/// the same resource from gamepad bindings). While a rebind capture runs, and until the press it
/// took comes up ([`regia_input::InputRebinder::pending`], [`regia_input::InputRebinder::click_guard`]),
/// the actions are dropped: the capture owns those keys and pad buttons.
pub fn ui_focus_navigation(
    mut actions: ResMut<UiNavActions>,
    // The held modifiers and the rebind capture ride with the key reader as one parameter: the
    // system is close to Bevy's 16-parameter limit once the other UI lanes add theirs.
    (mut keys, mut held, capture, menu): (
        MessageReader<regia_platform::KeyboardInput>,
        Local<crate::interaction::UiModifiers>,
        Option<Res<regia_input::InputRebinder>>,
        Option<Res<MenuNavState>>,
    ),
    // The alias spells the query data `'static`, as `navigate_graph` takes it: an elided
    // `&ComputedNode` here would be a lifetime of this function and not coerce.
    focusables: crate::focus_graph::FocusableQuery,
    scopes: Query<(Entity, Option<&crate::widgets::Modal>), With<FocusScope>>,
    defaults: Query<Entity, With<DefaultFocus>>,
    nav: crate::focus_graph::NavWorld,
    parents: Query<&ChildOf>,
    sliders: Query<(&crate::widgets::Slider, &ComputedNode), Without<ManualValueNav>>,
    mut mode: ResMut<UiInputMode>,
    mut state: ResMut<FocusState>,
    mut commands: Commands,
    mut events: MessageWriter<UiEvent>,
    layers: Res<crate::layers::InputLayers>,
    tooltips: Option<ResMut<crate::hints::TooltipState>>,
    text_inputs: Query<&crate::text_input::TextInput>,
) {
    let _span = tracing::trace_span!("ui_focus").entered();
    // A focused text input owns the arrow keys, Space and (multi-line) Enter for editing; Tab and
    // Escape still navigate, and so does the gamepad.
    let editing = state.focused.and_then(|f| text_inputs.get(f).ok());
    let menu_drives = menu.is_some_and(|m| m.active());
    for e in keys.read() {
        let down = e.state == regia_platform::ButtonState::Pressed;
        // With the `Menu` input context driving navigation (ADR-7070) the keys only track modifiers.
        if held.apply_key(&e.logical_key, down) || !down || menu_drives {
            continue;
        }
        let named = key_named(&e.logical_key);
        if let Some(input) = editing {
            let owned = match named {
                Some("left" | "right") => true,
                Some("up" | "down") => input.multiline,
                Some("submit") => true,
                _ => false,
            };
            if owned {
                continue;
            }
        }
        match named {
            Some("up") => actions.up = true,
            Some("down") => actions.down = true,
            Some("left") => actions.left = true,
            Some("right") => actions.right = true,
            Some("tab") => {
                // Winit names the key `Tab` whether or not Shift is down, so Shift comes from
                // the held modifier; `ui.type` sends `Shift+Tab` as a Tab with a Shift
                // physical key.
                if held.shift || e.physical_key.contains("Shift") {
                    actions.prev = true;
                } else {
                    actions.next = true;
                }
            }
            Some("submit") => actions.submit = true,
            Some("cancel") => actions.cancel = true,
            _ => {}
        }
    }
    if layers.mode() == crate::layers::LayerInputMode::GameOnly {
        // The top layer hands input to the game: the UI does not navigate.
        actions.clear();
        return;
    }
    if capture.is_some_and(|r| r.pending().is_some() || r.click_guard()) {
        // A rebind capture owns the keys and pad buttons until its press comes up: the Escape or
        // B it takes must not also close the dialog, nor a captured Tab or D-pad move focus off
        // the row being rebound. Screen-reader actions do not come through here.
        actions.clear();
        return;
    }
    if actions.up
        || actions.down
        || actions.left
        || actions.right
        || actions.next
        || actions.prev
        || actions.submit
    {
        // Keyboard and gamepad actions both arrive here; the pointer sets Pointer again.
        *mode = UiInputMode::Navigation;
    }
    if let Some(mut tooltips) = tooltips {
        // Activating or cancelling closes the tooltip of the focused node (hints.rs).
        if actions.cancel {
            tooltips.request_dismiss(crate::hints::TooltipDismiss::Cancel);
        } else if actions.submit {
            tooltips.request_dismiss(crate::hints::TooltipDismiss::Press);
        }
    }
    if state.focused.is_some_and(|focused| nav.subtree_disabled(focused)) {
        set_focus(None, &mut state, &mut commands, &mut events);
    }
    // Left/right on a focused slider change its value (a controller has no drag).
    if (actions.left || actions.right)
        && let Some(focused) = state.focused
        && let Ok((slider, node)) = sliders.get(focused)
    {
        let span = (slider.max - slider.min).abs().max(1e-6);
        let fraction = if slider.step > 0.0 {
            slider.step / span
        } else {
            0.05
        };
        let sign = if actions.right { 1.0 } else { -1.0 };
        actions.value_up = actions.right;
        actions.value_down = actions.left;
        events.write(UiEvent::new(
            focused,
            UiEventKind::Drag([sign * fraction * node.rect[2], 0.0]),
        ));
        actions.left = false;
        actions.right = false;
        actions.value_up = false;
        actions.value_down = false;
    }
    let dir = if actions.up {
        Some(NavDirection::Up)
    } else if actions.down {
        Some(NavDirection::Down)
    } else if actions.left {
        Some(NavDirection::Left)
    } else if actions.right {
        Some(NavDirection::Right)
    } else if actions.next {
        Some(NavDirection::Next)
    } else if actions.prev {
        Some(NavDirection::Prev)
    } else {
        None
    };
    // Refresh the active scope from open modal scopes.
    // The top input layer, when there is one, is the scope. Assign only when it differs: a
    // `ResMut` write marks the resource changed, and a quiet frame must not look like a focus move.
    let next_scope = layers.top_root().or_else(|| {
        // A closed modal keeps its `FocusScope` component but no longer traps focus.
        let open: Vec<Entity> = scopes
            .iter()
            .filter(|(_, modal)| scope_is_open(modal.map(|m| m.open)))
            .map(|(e, _)| e)
            .collect();
        pick_scope(&open, |s| state.focused.is_some_and(|f| in_scope(&parents, s, f)))
    });
    if state.scope != next_scope {
        state.scope = next_scope;
    }
    if let Some(dir) = dir {
        // The focus graph (ADR-516): groups, wrap/stop rules, Tab index, focus recovery.
        crate::focus_graph::navigate_graph(
            dir,
            &focusables,
            &defaults,
            &nav,
            &mut state,
            &mut commands,
            &mut events,
        );
    }
    if actions.submit
        && let Some(focused) = state.focused
        && sliders.get(focused).is_err()
    {
        events.write(UiEvent::new(focused, UiEventKind::Click));
    }
    if actions.cancel
        && let Some(scope) = state.scope
    {
        commands.queue(move |world: &mut World| {
            if !crate::screens::handle_screen_back(world, scope) && !crate::node::is_disabled(world, scope) {
                world.write_message(UiEvent::new(scope, UiEventKind::Click));
            }
        });
    }
    if (actions.value_up || actions.value_down)
        && let Some(focused) = state.focused
    {
        events.write(UiEvent::new(
            focused,
            UiEventKind::Drag([if actions.value_up { 1.0 } else { -1.0 }, 0.0]),
        ));
    }
    actions.clear();
}

/// Accepts winit-style names (`ArrowUp`, `Up`).
fn key_named(key: &str) -> Option<&'static str> {
    let lower = key.to_ascii_lowercase();
    match lower.as_str() {
        "up" | "arrowup" => Some("up"),
        "down" | "arrowdown" => Some("down"),
        "left" | "arrowleft" => Some("left"),
        "right" | "arrowright" => Some("right"),
        "tab" => Some("tab"),
        "enter" | "return" | "space" | "numpadenter" => Some("submit"),
        "escape" => Some("cancel"),
        _ => None,
    }
}

/// Most `ChildOf` links [`in_scope`] follows. A longer chain is outside the scope, so a corrupt
/// cycle cannot spin hit testing, drag-and-drop or scroll.
pub const MAX_ANCESTOR_STEPS: usize = 4096;

/// Whether `entity` is `scope` or a descendant of it.
///
/// A chain longer than [`MAX_ANCESTOR_STEPS`] is outside the scope.
pub fn in_scope(parents: &Query<&ChildOf>, scope: Entity, entity: Entity) -> bool {
    let mut current = entity;
    for _ in 0..MAX_ANCESTOR_STEPS {
        if current == scope {
            return true;
        }
        match parents.get(current) {
            Ok(parent) => current = parent.0,
            Err(_) => return false,
        }
    }
    false
}

/// Moves focus in `dir`, honouring scopes and explicit links; when nothing is focused yet the
/// first call focuses the default focus node (or the first focusable) without moving.
///
/// The geometry-and-links navigation from before the focus graph. [`ui_focus_navigation`] now
/// calls [`crate::focus_graph::navigate_graph`], which adds focus groups, wrap and stop rules,
/// Tab indices and recovery from stale focus; this function is kept for callers that drive
/// focus themselves.
pub fn navigate(
    dir: NavDirection,
    focusables: &Query<(Entity, &ComputedNode, &Focusable), Without<Disabled>>,
    defaults: &Query<Entity, With<DefaultFocus>>,
    parents: &Query<&ChildOf>,
    state: &mut FocusState,
    commands: &mut Commands,
    events: &mut MessageWriter<UiEvent>,
) {
    let scope = state.scope;
    let mut candidates: Vec<(Entity, [f32; 4], NavLinks)> = focusables
        .iter()
        .filter(|(e, c, _)| {
            c.rect[2] > 0.0 && c.rect[3] > 0.0 && scope.is_none_or(|s| in_scope(parents, s, *e))
        })
        .map(|(e, c, f)| (e, c.rect, f.nav))
        .collect();
    // Query order follows archetypes; sort so ties (equal scores, same corner) resolve the same
    // way on every run: reading order, then entity id.
    candidates.sort_by(|a, b| {
        (a.1[1], a.1[0])
            .partial_cmp(&(b.1[1], b.1[0]))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.to_bits().cmp(&b.0.to_bits()))
    });
    let current = state.focused.or_else(|| {
        defaults
            .iter()
            .min_by_key(|e| e.to_bits())
            .or_else(|| candidates.first().map(|(e, _, _)| *e))
    });
    let Some(current) = current else {
        return;
    };
    let Some((_, from_rect, nav)) = candidates.iter().find(|(e, _, _)| *e == current) else {
        return;
    };
    let from_rect = *from_rect;
    if state.focused.is_none() {
        // The first press only reveals the focus ring on the default node (a controller user
        // must see where they are before the first move).
        set_focus(Some(current), state, commands, events);
        return;
    }
    let next = match dir {
        NavDirection::Next | NavDirection::Prev => {
            // Reading order (top to bottom, then left to right): query iteration order depends
            // on archetypes, which would make Tab and the shoulder buttons jump around.
            let ordered: Vec<&(Entity, [f32; 4], NavLinks)> = candidates.iter().collect();
            let n = ordered.len();
            ordered.iter().position(|(e, _, _)| *e == current).map(|i| {
                let step = if dir == NavDirection::Next { 1 } else { n - 1 };
                ordered[(i + step) % n].0
            })
        }
        NavDirection::Up => nav.up,
        NavDirection::Down => nav.down,
        NavDirection::Left => nav.left,
        NavDirection::Right => nav.right,
    };
    let next = next.or_else(|| {
        candidates
            .iter()
            .filter(|(e, _, _)| *e != current)
            .filter_map(|(e, r, _)| nav_score(from_rect, *r, dir).map(|s| (s, *e)))
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(_, e)| e)
    });
    match next {
        Some(n) if n != current => set_focus(Some(n), state, commands, events),
        _ => {}
    }
}

/// Sets focus (writing `Focused` components and `FocusChanged` events).
pub fn set_focus(
    next: Option<Entity>,
    state: &mut FocusState,
    commands: &mut Commands,
    events: &mut MessageWriter<UiEvent>,
) {
    let prev = state.focused;
    if prev == next {
        return;
    }
    if let Some(p) = prev
        && let Ok(mut cmds) = commands.get_entity(p)
    {
        cmds.remove::<Focused>();
    }
    if let Some(n) = next
        && let Ok(mut cmds) = commands.get_entity(n)
    {
        cmds.insert(Focused);
    }
    if let Some(p) = prev {
        events.write(UiEvent::new(p, UiEventKind::FocusChanged(false)));
    }
    if let Some(n) = next {
        events.write(UiEvent::new(n, UiEventKind::FocusChanged(true)));
    }
    state.focused = next;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_modal_scope_does_not_trap_focus() {
        let mut world = World::new();
        let dialog = world.spawn_empty().id();
        assert!(!scope_is_open(Some(false)));
        assert!(scope_is_open(Some(true)));
        assert!(scope_is_open(None));
        // The only scope is a closed modal: no scope remains, so navigation is unrestricted.
        assert_eq!(pick_scope(&[], |_| false), None);
        assert_eq!(pick_scope(&[dialog], |_| false), Some(dialog));
    }

    #[test]
    fn prefers_aligned_overlap() {
        let from = [0.0, 0.0, 100.0, 50.0];
        let aligned = [10.0, 100.0, 80.0, 50.0]; // directly below
        let lateral = [300.0, 100.0, 80.0, 50.0]; // below but far right
        let a = nav_score(from, aligned, NavDirection::Down).unwrap();
        let b = nav_score(from, lateral, NavDirection::Down).unwrap();
        assert!(a < b, "aligned ({a}) beats lateral ({b})");
    }

    #[test]
    fn behind_is_rejected() {
        let from = [0.0, 0.0, 100.0, 50.0];
        let above = [0.0, -100.0, 100.0, 50.0];
        assert!(nav_score(from, above, NavDirection::Down).is_none());
        let left = [-100.0, 0.0, 50.0, 50.0];
        assert!(nav_score(from, left, NavDirection::Right).is_none());
    }

    #[test]
    fn closer_wins() {
        let from = [0.0, 0.0, 100.0, 50.0];
        let near = [0.0, 60.0, 100.0, 50.0];
        let far = [0.0, 500.0, 100.0, 50.0];
        let a = nav_score(from, near, NavDirection::Down).unwrap();
        let b = nav_score(from, far, NavDirection::Down).unwrap();
        assert!(a < b);
    }

    #[test]
    fn horizontal_directions() {
        let from = [0.0, 0.0, 100.0, 50.0];
        let right = [200.0, 0.0, 100.0, 50.0];
        assert!(nav_score(from, right, NavDirection::Right).is_some());
        assert!(nav_score(from, right, NavDirection::Left).is_none());
    }

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn stick_directions_use_the_dominant_axis() {
        let dz = 0.55;
        assert_eq!(stick_direction(0.3, 0.3, dz), None);
        assert_eq!(stick_direction(0.9, 0.2, dz), Some(NavDirection::Right));
        assert_eq!(stick_direction(-0.9, 0.2, dz), Some(NavDirection::Left));
        // Stick Y is up-positive in the platform's gamepad convention.
        assert_eq!(stick_direction(0.1, 0.8, dz), Some(NavDirection::Up));
        assert_eq!(stick_direction(0.1, -0.8, dz), Some(NavDirection::Down));
        assert_eq!(stick_direction(0.7, 0.7, dz), Some(NavDirection::Up));
    }

    #[test]
    fn non_finite_stick_axes_mean_no_direction() {
        let dz = 0.55;
        assert_eq!(stick_direction(f32::NAN, f32::NAN, dz), None);
        assert_eq!(stick_direction(f32::NAN, 0.9, dz), None);
        assert_eq!(stick_direction(0.9, f32::INFINITY, dz), None);
    }

    #[test]
    fn direction_repeat_fires_at_once_then_delays_then_repeats() {
        let cfg = GamepadNavConfig::default();
        let mut r = DirectionRepeat::default();
        let held = Some(NavDirection::Down);
        assert_eq!(r.step(held, 16 * MS, &cfg), held, "first press is immediate");
        let mut presses = 0;
        let mut t = Duration::ZERO;
        // Hold for one second at 60 fps: delay 350 ms, then one press per 120 ms.
        while t < Duration::from_secs(1) {
            if r.step(held, 16 * MS, &cfg).is_some() {
                presses += 1;
            }
            t += 16 * MS;
        }
        assert!(
            (5..=6).contains(&presses),
            "{presses} repeats in the first second"
        );
        assert_eq!(r.step(None, 16 * MS, &cfg), None);
        assert_eq!(r.step(held, 16 * MS, &cfg), held, "a new hold presses at once");
    }

    #[test]
    fn repeat_does_not_burst_after_a_long_frame_and_matches_across_frame_rates() {
        let cfg = GamepadNavConfig::default();
        let held = Some(NavDirection::Right);
        let count = |dt: Duration| {
            let mut r = DirectionRepeat::default();
            r.step(held, dt, &cfg);
            let (mut n, mut t) = (0, Duration::ZERO);
            while t < Duration::from_secs(2) {
                n += usize::from(r.step(held, dt, &cfg).is_some());
                t += dt;
            }
            n
        };
        let (at30, at60) = (count(33 * MS), count(16 * MS));
        assert!(at30.abs_diff(at60) <= 2, "30 fps {at30} vs 60 fps {at60}");
        let mut r = DirectionRepeat::default();
        r.step(held, MS, &cfg);
        // One 5 s hitch is one press, not forty.
        assert_eq!(r.step(held, Duration::from_secs(5), &cfg), held);
        assert_eq!(r.step(held, MS, &cfg), None);
    }

    #[test]
    fn ring_policy_follows_the_input_mode() {
        let nav = UiInputMode::Navigation;
        let pointer = UiInputMode::Pointer;
        assert!(FocusRingPolicy::default().shows_ring(nav));
        assert!(!FocusRingPolicy::default().shows_ring(pointer));
        assert!(FocusRingPolicy::Always.shows_ring(pointer));
        assert!(!FocusRingPolicy::Never.shows_ring(nav));
        assert_eq!(FocusRingPolicy::default().name(), "navigation_only");
    }

    #[test]
    fn ring_geometry_grows_by_offset_plus_width_and_stays_concentric() {
        let g = focus_ring_geometry([0.0, 0.0, 100.0, 40.0], 8.0, 2.0, 2.0).unwrap();
        assert_eq!(g.rect, [-4.0, -4.0, 108.0, 48.0]);
        // Inner edge radius = widget radius + offset (10), outer = that + width.
        assert_eq!(g.radius, 12.0);
        assert_eq!(g.width, 2.0);
        // A square widget still gets a rounded ring: the offset rounds its corners.
        let square = focus_ring_geometry([0.0, 0.0, 100.0, 40.0], 0.0, 2.0, 2.0).unwrap();
        assert_eq!(square.radius, 4.0);
        // A pill keeps a pill ring: the outer radius clamps to half the short side.
        let pill = focus_ring_geometry([0.0, 0.0, 40.0, 24.0], 9999.0, 2.0, 2.0).unwrap();
        assert_eq!(pill.radius, 16.0);
    }

    #[test]
    fn inset_ring_geometry_stays_inside_the_border_box() {
        let g = focus_ring_geometry([10.0, 20.0, 300.0, 44.0], 4.0, 2.0, -2.0).unwrap();
        assert_eq!(g.rect, [10.0, 20.0, 300.0, 44.0]);
        assert_eq!(g.radius, 4.0);
        // Deeper insets shrink the ring; the radius never goes negative.
        let deep = focus_ring_geometry([10.0, 20.0, 300.0, 44.0], 1.0, 2.0, -5.0).unwrap();
        assert_eq!(deep.rect, [13.0, 23.0, 294.0, 38.0]);
        assert_eq!(deep.radius, 0.0);
    }

    #[test]
    fn degenerate_rings_draw_nothing() {
        assert!(focus_ring_geometry([0.0, 0.0, 10.0, 10.0], 0.0, 0.0, 2.0).is_none());
        assert!(focus_ring_geometry([0.0, 0.0, 10.0, 10.0], 0.0, -1.0, 2.0).is_none());
        assert!(focus_ring_geometry([0.0, 0.0, 10.0, 10.0], 0.0, f32::NAN, 2.0).is_none());
        assert!(focus_ring_geometry([0.0, 0.0, 4.0, 4.0], 0.0, 2.0, -6.0).is_none());
        assert!(focus_ring_geometry([0.0, 0.0, f32::NAN, f32::NAN], 4.0, 2.0, 2.0).is_none());
    }

    #[test]
    fn key_names() {
        assert_eq!(key_named("ArrowUp"), Some("up"));
        assert_eq!(key_named("Up"), Some("up"));
        assert_eq!(key_named("Enter"), Some("submit"));
        assert_eq!(key_named("Escape"), Some("cancel"));
        assert_eq!(key_named("x"), None);
    }
}
