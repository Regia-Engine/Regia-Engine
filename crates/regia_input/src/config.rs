// SPDX-License-Identifier: MPL-2.0
//! The action-mapping configuration (`docs/specs/11-platform-and-input.md` § 7): actions, input
//! sources, modifiers, triggers, contexts and bindings, stored in the [`InputConfig`] resource.
//!
//! A config can be built in code (the builders below), parsed from RON ([`InputConfig::from_ron`],
//! the format of `config/input.ron`) and reshaped at runtime by rebinding
//! ([`crate::InputOverrides`], merged with [`InputConfig::merge_overrides`]).
//!
//! Value convention: all raw and modified values are `Vec3`. Button sources and most 1D sources
//! put the scalar on x. `GamepadAxis::LeftStickY` and `RightStickY` put it on y for every action
//! kind, so [`crate::Actions::axis1d`] (the x component) reads 0 until a [`Swizzle::Yxz`] moves y
//! onto x. `GamepadStick` keeps both components for every kind, including `Axis1D`. 2D sources use
//! x/y. Stick/axis directions: **x = right, y = up/forward**.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_platform::MouseButton;

use crate::device::{Devices, KeyCode};

/// The value type of an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ActionKind {
    /// A boolean: pressed or not (read with `Actions::pressed`).
    Button,
    /// A single scalar on the x axis (read with `Actions::axis1d`).
    Axis1D,
    /// A 2D vector, x = right, y = forward (read with `Actions::axis2d`).
    Axis2D,
    /// A 3D vector (read with `Actions::value`).
    Axis3D,
}

impl ActionKind {
    /// How bindings for this kind combine when several are active: buttons OR, axes add.
    pub(crate) fn combine(self, current: Vec3, incoming: Vec3) -> Vec3 {
        match self {
            Self::Button => {
                if incoming != Vec3::ZERO {
                    incoming
                } else {
                    current
                }
            }
            _ => current + incoming,
        }
    }

    /// Clamps a combined value to the kind's canonical form (buttons to 0/1).
    pub(crate) fn clamp(self, value: Vec3) -> Vec3 {
        match self {
            Self::Button => {
                if value != Vec3::ZERO {
                    Vec3::X
                } else {
                    Vec3::ZERO
                }
            }
            _ => value,
        }
    }
}

/// One of the two analog sticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Stick {
    /// The left stick.
    Left,
    /// The right stick.
    Right,
}

impl Stick {
    /// The X axis of this stick.
    pub fn x(self) -> regia_platform::GamepadAxis {
        match self {
            Self::Left => regia_platform::GamepadAxis::LeftStickX,
            Self::Right => regia_platform::GamepadAxis::RightStickX,
        }
    }

    /// The Y axis of this stick (up = +1).
    pub fn y(self) -> regia_platform::GamepadAxis {
        match self {
            Self::Left => regia_platform::GamepadAxis::LeftStickY,
            Self::Right => regia_platform::GamepadAxis::RightStickY,
        }
    }
}

/// The class of device an [`InputSource`] reads from; used by per-player device assignment
/// (`docs/specs/11` § 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceClass {
    /// Physical keyboard.
    Keyboard,
    /// Mouse (buttons, delta).
    Mouse,
    /// Any gamepad.
    Gamepad,
}

/// Where a binding's value comes from.
///
/// Every source coerces to every [`ActionKind`]: binary sources (keys, mouse buttons) are 0 or 1 on
/// the x axis for all kinds; a pad button is 0 or 1 for `Button` but keeps its analog `0..=1`
/// magnitude (a trigger pulled halfway reads 0.5) on the x axis for every other kind; `MouseDelta` reads its x component for `Axis1D` and its
/// full vector for `Axis2D`+; `Keys2D` reads its x component for `Axis1D` and reports "any key"
/// for `Button`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum InputSource {
    /// A single keyboard key.
    Key(KeyCode),
    /// Four keys mapped to up / down / left / right. The value is `(right - left, up - down)` in
    /// the action's axis convention (x = right, y = forward), normalised to length 1 when longer,
    /// so diagonal keys give a unit vector (`Axis2D` and `Axis3D` actions; z stays 0).
    Keys2D {
        /// Key read as forward (+y).
        up: KeyCode,
        /// Key read as backward (−y).
        down: KeyCode,
        /// Key read as left (−x).
        left: KeyCode,
        /// Key read as right (+x).
        right: KeyCode,
    },
    /// Mouse motion accumulated this frame ([`crate::Mouse::delta`]), in device units.
    MouseDelta,
    /// A mouse button.
    MouseButton(MouseButton),
    /// A gamepad button, any connected pad.
    GamepadButton(regia_platform::GamepadButton),
    /// One analog stick of a gamepad. Both components are returned for every [`ActionKind`],
    /// including `Axis1D` (y is not dropped).
    GamepadStick(Stick),
    /// A single gamepad axis (stick Y axes land on y, everything else on x), for every
    /// [`ActionKind`]. An `Axis1D` binding of stick Y reads 0 from [`crate::Actions::axis1d`]
    /// until [`Swizzle::Yxz`] moves y onto x. Rebind inserts that swizzle.
    GamepadAxis(regia_platform::GamepadAxis),
}

impl InputSource {
    /// A single keyboard key.
    pub fn key(key: impl Into<KeyCode>) -> Self {
        Self::Key(key.into())
    }

    /// A four-key 2D axis (see [`InputSource::Keys2D`]).
    pub fn keys2d(
        up: impl Into<KeyCode>,
        down: impl Into<KeyCode>,
        left: impl Into<KeyCode>,
        right: impl Into<KeyCode>,
    ) -> Self {
        Self::Keys2D {
            up: up.into(),
            down: down.into(),
            left: left.into(),
            right: right.into(),
        }
    }

    /// A mouse button.
    pub fn mouse_button(button: MouseButton) -> Self {
        Self::MouseButton(button)
    }

    /// A gamepad button.
    pub fn gamepad_button(button: regia_platform::GamepadButton) -> Self {
        Self::GamepadButton(button)
    }

    /// A gamepad stick.
    pub fn gamepad_stick(stick: Stick) -> Self {
        Self::GamepadStick(stick)
    }

    /// A gamepad axis directly.
    pub fn gamepad_axis(axis: regia_platform::GamepadAxis) -> Self {
        Self::GamepadAxis(axis)
    }

    /// The device class this source reads from.
    pub fn class(&self) -> DeviceClass {
        match self {
            Self::Key(_) | Self::Keys2D { .. } => DeviceClass::Keyboard,
            Self::MouseButton(_) | Self::MouseDelta => DeviceClass::Mouse,
            Self::GamepadButton(_) | Self::GamepadStick(_) | Self::GamepadAxis(_) => DeviceClass::Gamepad,
        }
    }

    /// The raw value of this source for an action of kind `kind`, before any modifiers. Button
    /// values are 0 or 1 on the x axis. Most 1D values are on the x axis with y/z zero.
    /// `GamepadAxis` stick Y is the exception: it is `(0, axis, 0)` for every kind, so an
    /// `Axis1D` reader of x sees 0 until [`Swizzle::Yxz`]. `GamepadStick` is `(x, y, 0)` for
    /// every kind.
    ///
    /// A press and a release inside one frame's batch still count as actuated for the rest of
    /// that frame (the device keeps the edge even though the final state is "up"), so a fast
    /// click is not dropped when it lands on a frame boundary (audit finding 2, ADR-4768); the
    /// release wins from the next batch that holds no press. The edge is a *frame* fact: every
    /// fixed step of a multi-step frame still sees the click, so a 5 ms click inside a 50 ms
    /// hitch frame reads as held for all of the frame's steps (~50 ms of sim time) — pinned in
    /// `tests/edges.rs::batch_click_reads_held_for_every_step_of_the_frame` (fixrev review,
    /// finding 7).
    pub fn raw(&self, kind: ActionKind, devices: &Devices) -> Vec3 {
        match self {
            Self::Key(key) => {
                binary(devices.keyboard.pressed(key.as_str()) || devices.keyboard.just_pressed(key.as_str()))
            }
            Self::MouseButton(button) => {
                binary(devices.mouse.pressed(*button) || devices.mouse.just_pressed(*button))
            }
            Self::MouseDelta => {
                let raw = devices.mouse.delta();
                let delta = Vec3::new(raw.x, raw.y, 0.0);
                match kind {
                    ActionKind::Button => binary(delta != Vec3::ZERO),
                    ActionKind::Axis1D => Vec3::new(delta.x, 0.0, 0.0),
                    _ => delta,
                }
            }
            Self::GamepadButton(button) => {
                let value = devices.max_button_value(*button);
                match kind {
                    ActionKind::Button => binary(value > 0.5),
                    ActionKind::Axis1D => Vec3::new(value, 0.0, 0.0),
                    _ => Vec3::new(value, 0.0, 0.0),
                }
            }
            Self::GamepadStick(stick) => Vec3::new(devices.axis(stick.x()), devices.axis(stick.y()), 0.0),
            Self::GamepadAxis(axis) => match axis {
                regia_platform::GamepadAxis::LeftStickY | regia_platform::GamepadAxis::RightStickY => {
                    Vec3::new(0.0, devices.axis(*axis), 0.0)
                }
                _ => Vec3::new(devices.axis(*axis), 0.0, 0.0),
            },
            Self::Keys2D {
                up,
                down,
                left,
                right,
            } => {
                let keyboard = devices.keyboard;
                let held_or_edge =
                    |key: &KeyCode| keyboard.pressed(key.as_str()) || keyboard.just_pressed(key.as_str());
                let axis = Vec2::new(
                    axis_component(held_or_edge(right), held_or_edge(left)),
                    axis_component(held_or_edge(up), held_or_edge(down)),
                );
                match kind {
                    ActionKind::Button => binary(axis != Vec2::ZERO),
                    ActionKind::Axis1D => Vec3::new(axis.x, 0.0, 0.0),
                    ActionKind::Axis2D | ActionKind::Axis3D if axis.length_squared() > 1.0 => {
                        Vec3::new(axis.x, axis.y, 0.0).normalize_or_zero()
                    }
                    _ => Vec3::new(axis.x, axis.y, 0.0),
                }
            }
        }
    }
}

/// `(1, 0, 0)` when active, zero otherwise.
fn binary(active: bool) -> Vec3 {
    Vec3::new(f32::from(u8::from(active)), 0.0, 0.0)
}

/// `1.0` when `positive`, `-1.0` when `negative`, `0.0` when neither or both.
fn axis_component(positive: bool, negative: bool) -> f32 {
    f32::from(u8::from(positive)) - f32::from(u8::from(negative))
}

/// Whether a [`DeadZone`] clamps the vector's length or each component separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum DeadZoneMode {
    /// Clamp each component independently (suits per-axis sticks).
    #[default]
    Axial,
    /// Clamp the vector's length, keeping its direction (suits sticks and mouse deltas).
    Radial,
}

/// Zeroes small values: at or below `lo` the value is zero, at or above `hi` it is full scale,
/// and in between the magnitude is rescaled linearly from `[lo, hi]` to `[0, 1]`. Expected usage
/// is `0.0 <= lo <= hi <= 1.0`; a degenerate `lo == hi` acts as a hard threshold.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeadZone {
    /// Axial or radial clamping.
    pub mode: DeadZoneMode,
    /// Lower edge: at or below this magnitude the value is zero.
    pub lo: f32,
    /// Upper edge: at or above this magnitude the value is full scale.
    pub hi: f32,
}

impl DeadZone {
    /// A radial dead zone (length-based, direction preserved).
    pub fn radial(lo: f32, hi: f32) -> Self {
        Self {
            mode: DeadZoneMode::Radial,
            lo,
            hi,
        }
    }

    /// An axial dead zone (per component).
    pub fn axial(lo: f32, hi: f32) -> Self {
        Self {
            mode: DeadZoneMode::Axial,
            lo,
            hi,
        }
    }

    /// Applies the dead zone to `value`.
    pub fn apply(&self, value: Vec3) -> Vec3 {
        match self.mode {
            DeadZoneMode::Axial => Vec3::new(
                rescale_component(value.x, self.lo, self.hi),
                rescale_component(value.y, self.lo, self.hi),
                rescale_component(value.z, self.lo, self.hi),
            ),
            DeadZoneMode::Radial => {
                let length = value.length();
                let span = self.hi - self.lo;
                if length >= self.hi {
                    // A real span saturates at full scale; a degenerate one is a pure threshold.
                    return if span > 0.0 {
                        value.normalize_or_zero()
                    } else {
                        value
                    };
                }
                if length == 0.0 || length <= self.lo {
                    return Vec3::ZERO;
                }
                if span <= 0.0 {
                    return value;
                }
                value * ((length - self.lo) / span / length)
            }
        }
    }
}

/// Rescales one component's magnitude from `[lo, hi]` to `[0, 1]`, preserving its sign.
fn rescale_component(component: f32, lo: f32, hi: f32) -> f32 {
    let magnitude = component.abs();
    let span = hi - lo;
    if magnitude >= hi {
        // A real span saturates at full scale; a degenerate one is a pure threshold.
        return if span > 0.0 { component.signum() } else { component };
    }
    if magnitude == 0.0 || magnitude <= lo {
        return 0.0;
    }
    if span <= 0.0 {
        return component;
    }
    component * ((magnitude - lo) / span / magnitude)
}

/// Axis permutations for [`Modifier::Swizzle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Swizzle {
    /// Keep `(x, y, z)`.
    Xyz,
    /// `(y, x, z)`: swap the first two axes (put a stick's y on x for an `Axis1D` action).
    Yxz,
    /// `(z, x, y)`.
    Zxy,
    /// `(x, z, y)`.
    Xzy,
}

impl Swizzle {
    /// Permutes `value`'s axes.
    pub fn apply(self, value: Vec3) -> Vec3 {
        match self {
            Self::Xyz => value,
            Self::Yxz => Vec3::new(value.y, value.x, value.z),
            Self::Zxy => Vec3::new(value.z, value.x, value.y),
            Self::Xzy => Vec3::new(value.x, value.z, value.y),
        }
    }
}

/// Response curves for [`Modifier::ResponseCurve`]: how a raw stick deflection maps to the
/// action's value, applied to each axis's magnitude.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ResponseCurve {
    /// Identity: pass the value through.
    Linear,
    /// `v²` (fine control near centre).
    Square,
    /// `v³` (very fine near centre).
    Cubic,
    /// `sqrt(v)` (fast response near centre).
    SquareRoot,
    /// Smooth S-curve: `3v² − 2v³`.
    SmoothStep,
}

impl ResponseCurve {
    /// Evaluates the curve at `t`, preserving `t`'s sign.
    pub fn apply(self, t: f32) -> f32 {
        let magnitude = t.abs();
        let curved = match self {
            Self::Linear => magnitude,
            Self::Square => magnitude * magnitude,
            Self::Cubic => magnitude * magnitude * magnitude,
            Self::SquareRoot => magnitude.sqrt(),
            // Clamped: the polynomial falls past 1 and would flip the sign for inputs above 1.5.
            Self::SmoothStep => {
                let m = magnitude.min(1.0);
                m * m * (3.0 - 2.0 * m)
            }
        };
        t.signum() * curved
    }
}

/// A transform applied to a binding's raw value. Modifiers are applied in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Modifier {
    /// Zeroes small values (see [`DeadZone`]).
    DeadZone(DeadZone),
    /// Multiplies each component (e.g. look sensitivity).
    Scale(Vec3),
    /// Negates the selected components.
    Negate {
        /// Negate x.
        x: bool,
        /// Negate y.
        y: bool,
        /// Negate z.
        z: bool,
    },
    /// Reorders the components (see [`Swizzle`]).
    Swizzle(Swizzle),
    /// Negates y when `true`; a readability shorthand for flight-stick / camera inversion.
    InvertY(bool),
    /// Exponential smoothing toward the raw value. The parameter is the recovery rate in 1/s:
    /// larger reacts faster; `0.0` disables smoothing. Needs frame time
    /// ([`ModifierFrame::delta_secs`]).
    Smooth(f32),
    /// Remaps each component's magnitude through a fixed curve (see [`ResponseCurve`]).
    ResponseCurve(ResponseCurve),
    /// Multiplies by the frame's delta seconds: scales a per-second rate (speed, turn rate) into
    /// the amount to apply this frame, so the result is framerate independent. Do not use it on a
    /// value that is already per frame, such as a raw mouse delta.
    ScaleByDelta,
}

impl Modifier {
    /// Applies this modifier to `value`; `frame` carries the time and per-binding state the
    /// time-dependent modifiers need.
    pub fn apply(&self, value: Vec3, frame: &mut ModifierFrame<'_>) -> Vec3 {
        // A non-finite input or result (NaN/inf from a bad scale or a device overflow) is treated
        // as a resting source, never as an actuated one.
        if !value.is_finite() {
            return Vec3::ZERO;
        }
        let result = self.apply_finite(value, frame);
        if result.is_finite() { result } else { Vec3::ZERO }
    }

    fn apply_finite(&self, value: Vec3, frame: &mut ModifierFrame<'_>) -> Vec3 {
        match self {
            Self::DeadZone(dead_zone) => dead_zone.apply(value),
            Self::Scale(scale) => value * *scale,
            Self::Negate { x, y, z } => Vec3::new(
                if *x { -value.x } else { value.x },
                if *y { -value.y } else { value.y },
                if *z { -value.z } else { value.z },
            ),
            Self::Swizzle(swizzle) => swizzle.apply(value),
            Self::InvertY(invert) => Vec3::new(value.x, if *invert { -value.y } else { value.y }, value.z),
            Self::Smooth(rate) => {
                let rate = if rate.is_finite() { rate.max(0.0) } else { 0.0 };
                if rate == 0.0 {
                    return value;
                }
                // A zero-step frame evaluates with dt = 0. Hold the previous sample so look
                // smoothing does not flicker onto the raw value for that frame. With no stored
                // sample, pass the raw value through and allocate nothing.
                if !frame.delta_secs.is_finite() || frame.delta_secs <= 0.0 {
                    if let Some(previous) = frame.smoothed.get(&frame.binding)
                        && previous.is_finite()
                    {
                        return *previous;
                    }
                    return value;
                }
                // Exponential decay toward the raw value; time-constant form so the result does not
                // depend on the frame rate. The smoothing state is per binding. The decay uses
                // the deterministic exp (multiplies only, bit-identical on every target), so a
                // replay or a lockstep checksum that re-evaluates the binding from the recorded
                // samples cannot diverge per OS (audit finding 6, ADR-4768).
                let mut previous = *frame.smoothed.entry(frame.binding.clone()).or_insert(value);
                if !previous.is_finite() {
                    previous = value;
                }
                let blend = 1.0 - crate::deterministic::exp_f32(-rate * frame.delta_secs);
                let smoothed = previous.lerp(value, blend.clamp(0.0, 1.0));
                frame.smoothed.insert(frame.binding.clone(), smoothed);
                smoothed
            }
            Self::ResponseCurve(curve) => {
                Vec3::new(curve.apply(value.x), curve.apply(value.y), curve.apply(value.z))
            }
            Self::ScaleByDelta => value * frame.delta_secs,
        }
    }
}

impl From<DeadZone> for Modifier {
    fn from(dead_zone: DeadZone) -> Self {
        Self::DeadZone(dead_zone)
    }
}

impl From<Vec3> for Modifier {
    fn from(scale: Vec3) -> Self {
        Self::Scale(scale)
    }
}

/// The per-frame context modifiers are evaluated in: frame time and the per-binding smoothing
/// state (owned by [`crate::Actions`], carried between frames).
#[derive(Debug)]
pub struct ModifierFrame<'a> {
    /// Seconds the previous frame took; 0 in tests that want time-independent modifiers.
    pub delta_secs: f32,
    /// Which binding is being evaluated (keyed by context name and binding index).
    pub binding: BindingKey,
    /// Smoothed values carried between frames.
    pub smoothed: &'a mut BTreeMap<BindingKey, Vec3>,
}

/// Identifies one binding inside a config for one player: player + context name + index.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BindingKey {
    /// The player whose evaluation owns the smoothing state (split-screen players must not
    /// share it).
    pub player: u8,
    /// The context the binding lives in.
    pub context: String,
    /// The binding's index within the context.
    pub index: usize,
}

/// Applies `modifiers` to `value` in order (first to last).
pub fn apply_modifiers(modifiers: &[Modifier], mut value: Vec3, frame: &mut ModifierFrame<'_>) -> Vec3 {
    for modifier in modifiers {
        value = modifier.apply(value, frame);
    }
    value
}

/// When an actuated binding counts as an active action (`docs/specs/11` § 7). A binding with no
/// triggers uses [`Trigger::Down`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Trigger {
    /// Active while actuated (the default).
    Down,
    /// Fires on the frame the action becomes actuated.
    Pressed,
    /// Fires on the frame the action stops being actuated.
    Released,
    /// Fires once the action has been held for this many seconds.
    Hold(f32),
    /// Fires when the action is released within this many seconds of starting (a quick tap).
    Tap(f32),
    /// While actuated, fires immediately and then every this many seconds.
    Pulse(f32),
    /// Active while this action is actuated and the named action's previous-frame
    /// [`crate::ActionState::actuated`] flag is set. That flag is true only when the partner's
    /// previous phase was `Started`, `Ongoing` or `Triggered` and its value was non-zero. It is
    /// not the partner's raw held value: a partner whose trigger is [`Trigger::Pressed`] is
    /// actuated for one frame, so a chord on a later frame stays [`crate::ActionPhase::Idle`]
    /// even while the partner key is still down. When the flag is false the phase is `Idle`,
    /// including when this chord was `Ongoing` on the previous frame (it does not pass through
    /// `Completed`). The rising edge is the chord becoming satisfied (this binding is down and the
    /// partner was actuated last frame), not the key going down while the chord was idle, so
    /// `just_pressed` is true on the first frame the chord reads as pressed. The previous frame is
    /// what is read, so evaluation does not depend on action order. Bind a `"Ctrl"` action with the
    /// default [`Trigger::Down`], then chord it onto the number-key actions.
    Chord(String),
}

/// One binding: writes the value of an [`InputSource`] (after its [`Modifier`]s) to an action.
///
/// Bindings of one action are alternatives. Each binding's triggers see that binding's own
/// value (an empty trigger list is [`Trigger::Down`] for that binding). Buttons keep their own
/// press edge, so a `Pressed` binding still fires while a sibling `Down` binding is held.
/// Trigger timers (tap, hold, pulse) belong to the action: a binding that is up, and was up,
/// does not clear or advance them. The action's output value is the combination of the
/// non-zero binding values (buttons keep the last non-zero value, axes add).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Binding {
    /// Name of the action this binding writes.
    pub action: String,
    /// Where the value comes from.
    pub input: InputSource,
    /// Modifiers applied to the raw value in order, before it is written to the action.
    #[serde(default)]
    pub modifiers: Vec<Modifier>,
    /// Triggers for this binding. Empty means [`Trigger::Down`] for this binding alone.
    #[serde(default)]
    pub triggers: Vec<Trigger>,
}

impl Binding {
    /// A binding without modifiers or triggers.
    pub fn new(action: impl Into<String>, input: InputSource) -> Self {
        Self {
            action: action.into(),
            input,
            modifiers: Vec::new(),
            triggers: Vec::new(),
        }
    }

    /// Adds one modifier, applied after those already present. Builder style.
    pub fn with_modifier(mut self, modifier: impl Into<Modifier>) -> Self {
        self.modifiers.push(modifier.into());
        self
    }

    /// Adds one trigger. Builder style.
    pub fn with_trigger(mut self, trigger: Trigger) -> Self {
        self.triggers.push(trigger);
        self
    }
}

/// A named group of bindings evaluated as one unit (`docs/specs/11` § 7). Contexts are pushed on
/// a per-player stack ([`crate::InputContexts`]); higher `priority` evaluates first, and a
/// context with `consume_lower` hides its actuated inputs from lower-priority contexts.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InputContext {
    /// Evaluation order: higher first.
    pub priority: i32,
    /// Whether this context's actuated inputs are hidden from lower-priority contexts.
    pub consume_lower: bool,
    /// The bindings of this context, evaluated in order.
    #[serde(default)]
    pub bindings: Vec<Binding>,
}

impl InputContext {
    /// A context with the given priority that does not consume lower contexts' inputs: an input
    /// bound in both this and a lower context fires in both. Use [`InputContext::consuming`] for
    /// "higher priority wins".
    pub fn new(priority: i32) -> Self {
        Self {
            priority,
            consume_lower: false,
            bindings: Vec::new(),
        }
    }

    /// A context with the given priority that hides its actuated inputs from lower contexts.
    pub fn consuming(priority: i32) -> Self {
        Self {
            priority,
            consume_lower: true,
            bindings: Vec::new(),
        }
    }

    /// Appends a binding. Builder style.
    pub fn with_binding(mut self, binding: Binding) -> Self {
        self.bindings.push(binding);
        self
    }
}

/// The default context name used by [`InputConfig::binding`].
pub const GAMEPLAY_CONTEXT: &str = "Gameplay";

/// The action-mapping configuration (spec 11 § 7): the declared actions and the contexts that
/// bind them. The [`Resource`] installed by [`crate::InputPlugin`].
///
/// # Example
///
/// ```
/// use regia_input::prelude::*;
///
/// let config = InputConfig::new()
///     .action("Jump", ActionKind::Button)
///     .binding(Binding::new("Jump", InputSource::key("Space")));
/// assert_eq!(config.kind("Jump"), Some(ActionKind::Button));
/// assert_eq!(config.gameplay().len(), 1);
/// ```
#[derive(Resource, Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InputConfig {
    /// The actions this config declares, by name and value kind.
    #[serde(default)]
    pub actions: BTreeMap<String, ActionKind>,
    /// All contexts by name. Which are enabled is per-player runtime state
    /// ([`crate::InputContexts`]).
    #[serde(default)]
    pub contexts: BTreeMap<String, InputContext>,
    /// The user's SpaceMouse overrides (`docs/specs/29-spacemouse.md` § 2.7): the only three
    /// settings an app may expose. Omitted from the RON while it holds the defaults.
    #[serde(default, skip_serializing_if = "SpaceMouseConfig::is_default")]
    pub spacemouse: SpaceMouseConfig,
    /// Action names whose combined device value is scaled by [`LookSettings`] (the player's
    /// look sensitivity and Y invert). Empty (the default, and older files) applies nothing:
    /// `GameSettings.input.sensitivity` and `invert_y` stay for the game to read (ADR-3464).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub look_actions: Vec<String>,
}

/// The player's look scale, copied from `GameSettings.input` by the settings plugin.
///
/// [`InputConfig::look_actions`] names the actions this scales. With that list empty the engine
/// does not read it, and the game reads the settings fields itself.
///
/// # Example
///
/// ```
/// use regia_core::math::Vec3;
/// use regia_input::LookSettings;
///
/// let look = LookSettings { sensitivity: 2.0, invert_y: true };
/// assert_eq!(look.apply(Vec3::new(1.0, 0.5, 0.0)), Vec3::new(2.0, -1.0, 0.0));
/// ```
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct LookSettings {
    /// Multiplier on the action value. Non-finite reads as `1`; applied in `0..=SENSITIVITY_MAX`.
    pub sensitivity: f32,
    /// Negate the action's Y after scaling.
    pub invert_y: bool,
}

impl LookSettings {
    /// The highest sensitivity the engine applies. A larger stored value is clamped when applied.
    pub const SENSITIVITY_MAX: f32 = 100.0;

    /// `sensitivity` made finite and clamped. A non-finite value reads as `1`.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let sensitivity = if self.sensitivity.is_finite() {
            self.sensitivity.clamp(0.0, Self::SENSITIVITY_MAX)
        } else {
            1.0
        };
        Self {
            sensitivity,
            invert_y: self.invert_y,
        }
    }

    /// Scales `value` and, when [`Self::invert_y`] is set, negates Y.
    #[must_use]
    pub fn apply(self, value: Vec3) -> Vec3 {
        let settings = self.sanitized();
        let mut out = value * settings.sensitivity;
        if settings.invert_y {
            out.y = -out.y;
        }
        out
    }
}

impl Default for LookSettings {
    fn default() -> Self {
        Self {
            sensitivity: 1.0,
            invert_y: false,
        }
    }
}

/// The per-app SpaceMouse overrides (`docs/specs/29-spacemouse.md` § 2.7), the `spacemouse`
/// section of the input settings RON. Dead zone, curve, modes, button actions and speeds are
/// fixed (`NavigationDefaults` in `regia_input::spacemouse`) and deliberately not here.
///
/// ```
/// use regia_input::{InputConfig, SpaceMouseConfig};
///
/// let config = InputConfig::from_ron("(spacemouse: (sensitivity: 9.0))").expect("parses");
/// assert_eq!(config.spacemouse.sanitized().sensitivity, SpaceMouseConfig::SENSITIVITY_MAX);
/// assert!(config.spacemouse.enabled, "unspecified fields keep their defaults");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SpaceMouseConfig {
    /// Whether the app reads SpaceMouse devices at all (default true). When false no device is
    /// opened and the merged state stays zero.
    pub enabled: bool,
    /// A multiplier on translation and rotation speed, default 1.0, clamped to
    /// [`Self::SENSITIVITY_MIN`]..=[`Self::SENSITIVITY_MAX`].
    pub sensitivity: f32,
    /// Per-axis sign flips in the order `x, y, z, rx, ry, rz` (default all false).
    pub invert: [bool; 6],
}

impl SpaceMouseConfig {
    /// The lowest accepted sensitivity.
    pub const SENSITIVITY_MIN: f32 = 0.1;
    /// The highest accepted sensitivity.
    pub const SENSITIVITY_MAX: f32 = 4.0;
    /// The out-of-the-box config: enabled, sensitivity 1.0, nothing inverted.
    pub const DEFAULT: Self = Self {
        enabled: true,
        sensitivity: 1.0,
        invert: [false; 6],
    };

    /// True when every field holds its default (the RON section is then omitted).
    pub fn is_default(&self) -> bool {
        *self == Self::DEFAULT
    }

    /// The config with `sensitivity` clamped to its range (a non-finite value becomes 1.0).
    #[must_use]
    pub fn sanitized(self) -> Self {
        let sensitivity = if self.sensitivity.is_finite() {
            self.sensitivity
                .clamp(Self::SENSITIVITY_MIN, Self::SENSITIVITY_MAX)
        } else {
            1.0
        };
        Self { sensitivity, ..self }
    }
}

impl Default for SpaceMouseConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl InputConfig {
    /// An empty config.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares an action. Builder style.
    pub fn action(mut self, name: impl Into<String>, kind: ActionKind) -> Self {
        self.actions.insert(name.into(), kind);
        self
    }

    /// Names `action` as a look action ([`InputConfig::look_actions`]). Builder style.
    /// Naming none leaves the player's sensitivity and Y invert for the game to read.
    pub fn look_action(mut self, action: impl Into<String>) -> Self {
        let action = action.into();
        if !self.look_actions.iter().any(|name| name == &action) {
            self.look_actions.push(action);
        }
        self
    }

    /// Adds (or replaces) a context. Builder style.
    pub fn context(mut self, name: impl Into<String>, context: InputContext) -> Self {
        self.contexts.insert(name.into(), context);
        self
    }

    /// Appends a binding to the default `Gameplay` context, creating the context if needed.
    /// Builder style.
    pub fn binding(mut self, binding: Binding) -> Self {
        self.push_binding(GAMEPLAY_CONTEXT, binding);
        self
    }

    /// Appends a binding to an arbitrary context, creating it with priority 0 if needed.
    /// Builder style.
    pub fn binding_in(mut self, context: impl Into<String>, binding: Binding) -> Self {
        self.push_binding(&context.into(), binding);
        self
    }

    fn push_binding(&mut self, context: &str, binding: Binding) {
        self.contexts
            .entry(context.to_string())
            .or_insert_with(|| InputContext::new(0))
            .bindings
            .push(binding);
    }

    /// The bindings of the default `Gameplay` context.
    pub fn gameplay(&self) -> &[Binding] {
        self.contexts
            .get(GAMEPLAY_CONTEXT)
            .map(|c| c.bindings.as_slice())
            .unwrap_or(&[])
    }

    /// The kind of a declared action, if it exists.
    pub fn kind(&self, name: &str) -> Option<ActionKind> {
        self.actions.get(name).copied()
    }

    /// Parses a config from RON (`config/input.ron`). Errors carry the source snippet.
    pub fn from_ron(ron: &str) -> Result<Self, String> {
        ron::from_str(ron).map_err(|e| format!("input config RON: {e}"))
    }

    /// Serialises the config to canonical RON, returning the serialisation error.
    ///
    /// [`InputConfig::to_ron`] swallows a serialisation failure and returns an empty string; this
    /// variant surfaces it. Prefer this when the config must round-trip losslessly.
    pub fn try_to_ron(&self) -> Result<String, ron::Error> {
        ron::ser::to_string_pretty(
            self,
            regia_core::ron_pretty().new_line(std::borrow::Cow::Borrowed("\n")),
        )
    }

    /// Serialises the config to canonical RON. A serialisation failure returns an empty string;
    /// use [`InputConfig::try_to_ron`] to surface the error instead.
    pub fn to_ron(&self) -> String {
        self.try_to_ron().unwrap_or_default()
    }

    /// Merges rebinding overrides on top of this config: for each overridden
    /// `(context, action)`, every binding of that action in that context is replaced by the
    /// override's single binding; each per-device override
    /// ([`InputOverrides::slots`](crate::InputOverrides::slots)) then replaces only the bindings on
    /// its slot's devices. Returns how many overrides were applied.
    pub fn merge_overrides(&mut self, overrides: &crate::rebind::InputOverrides) -> usize {
        let mut applied = 0;
        for (context, actions) in &overrides.contexts {
            let Some(ctx) = self.contexts.get_mut(context) else {
                tracing::warn!(
                    target: "regia_input",
                    "input override for unknown context {context:?}; ignored"
                );
                continue;
            };
            for (action, binding) in actions {
                let count = ctx.bindings.len();
                ctx.bindings.retain(|b| &b.action != action);
                if ctx.bindings.len() < count {
                    applied += 1;
                }
                ctx.bindings.push(binding.clone());
                // A chord's helper action must be declared for the chord to evaluate.
                for trigger in &binding.triggers {
                    if let Trigger::Chord(helper) = trigger {
                        self.actions.entry(helper.clone()).or_insert(ActionKind::Button);
                    }
                }
            }
        }
        for (context, actions) in &overrides.slots {
            let Some(ctx) = self.contexts.get_mut(context) else {
                tracing::warn!(
                    target: "regia_input",
                    "input override for unknown context {context:?}; ignored"
                );
                continue;
            };
            for (action, by_slot) in actions {
                for (slot, bindings) in by_slot {
                    let count = ctx.bindings.len();
                    ctx.bindings
                        .retain(|b| !(&b.action == action && slot.holds(&b.input)));
                    if ctx.bindings.len() < count {
                        applied += 1;
                    }
                    for binding in bindings.iter().filter(|b| slot.holds(&b.input)) {
                        ctx.bindings.push(binding.clone());
                        for trigger in &binding.triggers {
                            if let Trigger::Chord(helper) = trigger {
                                self.actions.entry(helper.clone()).or_insert(ActionKind::Button);
                            }
                        }
                    }
                }
            }
        }
        applied
    }
}

/// The input action that skips a skippable fullscreen video and the boot movies
/// (`docs/specs/50-video-playback.md` § 2.7, ADR-2410).
pub const SKIP_VIDEO_ACTION: &str = "skip_video";

/// The context of the default [`SKIP_VIDEO_ACTION`] bindings: enabled for player 0 while a skippable
/// video plays, consuming (priority 100), so the skip press does not also reach gameplay.
pub const VIDEO_CONTEXT: &str = "Video";

impl InputConfig {
    /// The default [`SKIP_VIDEO_ACTION`] bindings: Escape, Space and Enter, and the gamepad's South
    /// and Start buttons, in [`VIDEO_CONTEXT`]. Added only when the config binds the action nowhere,
    /// so a project's own binding (or a player's rebinding) wins; declares the action if needed.
    #[must_use]
    pub fn with_skip_video_default(mut self) -> Self {
        self.actions
            .entry(SKIP_VIDEO_ACTION.to_string())
            .or_insert(ActionKind::Button);
        let bound = self
            .contexts
            .values()
            .any(|c| c.bindings.iter().any(|b| b.action == SKIP_VIDEO_ACTION));
        if !bound {
            let mut context = self
                .contexts
                .remove(VIDEO_CONTEXT)
                .unwrap_or_else(|| InputContext::consuming(100));
            for input in [
                InputSource::key("Escape"),
                InputSource::key("Space"),
                InputSource::key("Enter"),
                InputSource::gamepad_button(regia_platform::GamepadButton::South),
                InputSource::gamepad_button(regia_platform::GamepadButton::Start),
            ] {
                context.bindings.push(Binding::new(SKIP_VIDEO_ACTION, input));
            }
            self.contexts.insert(VIDEO_CONTEXT.to_string(), context);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{Keyboard, Mouse};

    #[test]
    fn non_finite_modifier_results_are_zero_not_actuated() {
        let mut smoothed = Default::default();
        let mut frame = ModifierFrame {
            delta_secs: 0.016,
            smoothed: &mut smoothed,
            binding: BindingKey::default(),
        };
        let scaled = Modifier::Scale(Vec3::new(f32::INFINITY, 1.0, 1.0)).apply(Vec3::ZERO, &mut frame);
        assert_eq!(scaled, Vec3::ZERO);
        let poisoned = Modifier::Smooth(10.0).apply(Vec3::new(f32::NAN, 0.0, 0.0), &mut frame);
        assert_eq!(poisoned, Vec3::ZERO);
        let next = Modifier::Smooth(10.0).apply(Vec3::new(0.5, 0.0, 0.0), &mut frame);
        assert!(next.is_finite());
    }

    #[test]
    fn smooth_step_does_not_flip_sign_past_one() {
        for m in [1.0_f32, 1.5, 2.0, 100.0] {
            assert_eq!(ResponseCurve::SmoothStep.apply(m), 1.0);
            assert_eq!(ResponseCurve::SmoothStep.apply(-m), -1.0);
        }
    }

    /// A `Keyboard` with the given keys held.
    fn keyboard(held: &[&str]) -> Keyboard {
        Keyboard {
            held: held.iter().map(|key| KeyCode::new(*key)).collect(),
            ..Keyboard::default()
        }
    }

    /// Runs `modifier.apply` with no time and no smoothing state.
    fn apply_pure(modifier: &Modifier, value: Vec3) -> Vec3 {
        let mut smoothed = BTreeMap::new();
        let mut frame = ModifierFrame {
            delta_secs: 0.0,
            binding: BindingKey::default(),
            smoothed: &mut smoothed,
        };
        modifier.apply(value, &mut frame)
    }

    #[test]
    fn dead_zone_radial_zeroes_below_lo_and_rescales_between() {
        let dead_zone = DeadZone::radial(0.2, 1.0);
        assert_eq!(dead_zone.apply(Vec3::new(0.1, 0.0, 0.0)), Vec3::ZERO);
        assert_eq!(dead_zone.apply(Vec3::ZERO), Vec3::ZERO);
        assert_eq!(
            dead_zone.apply(Vec3::new(1.0, 0.0, 0.0)),
            Vec3::new(1.0, 0.0, 0.0)
        );
        let half = dead_zone.apply(Vec3::new(0.6, 0.0, 0.0));
        assert!((half.x - 0.5).abs() < 1e-6, "{half:?}");
        assert!((half.length() - 0.5).abs() < 1e-6, "direction preserved");
    }

    #[test]
    fn dead_zone_axial_works_per_component() {
        let dead_zone = DeadZone::axial(0.2, 1.0);
        assert_eq!(
            dead_zone.apply(Vec3::new(0.1, 1.0, 0.0)),
            Vec3::new(0.0, 1.0, 0.0)
        );
        let quarter = dead_zone.apply(Vec3::new(-0.4, 0.0, 0.2));
        assert!((quarter.x + 0.25).abs() < 1e-6, "{quarter:?}");
        assert!((quarter.z - 0.0).abs() < 1e-6, "z below lo zeroes");
    }

    #[test]
    fn dead_zone_saturates_at_full_scale_above_hi() {
        let radial = DeadZone::radial(0.15, 0.95);
        let diagonal = radial.apply(Vec3::new(1.0, 1.0, 0.0));
        assert!((diagonal.length() - 1.0).abs() < 1e-6, "{diagonal:?}");
        let at_hi = radial.apply(Vec3::new(0.95, 0.0, 0.0));
        let near_hi = radial.apply(Vec3::new(0.949, 0.0, 0.0));
        assert!(
            (at_hi.x - 1.0).abs() < 1e-6 && near_hi.x > 0.99,
            "{at_hi:?} {near_hi:?}"
        );
        let axial = DeadZone::axial(0.1, 0.9);
        assert_eq!(axial.apply(Vec3::new(-0.95, 0.9, 0.0)), Vec3::new(-1.0, 1.0, 0.0));
    }

    #[test]
    fn dead_zone_degenerate_span_is_a_threshold() {
        let dead_zone = DeadZone {
            mode: DeadZoneMode::Radial,
            lo: 0.5,
            hi: 0.5,
        };
        assert_eq!(dead_zone.apply(Vec3::new(0.4, 0.0, 0.0)), Vec3::ZERO);
        assert_eq!(
            dead_zone.apply(Vec3::new(0.5, 0.0, 0.0)),
            Vec3::new(0.5, 0.0, 0.0)
        );
        let axial = DeadZone {
            mode: DeadZoneMode::Axial,
            lo: 0.5,
            hi: 0.5,
        };
        assert_eq!(axial.apply(Vec3::new(0.4, 0.6, 0.0)), Vec3::new(0.0, 0.6, 0.0));
    }

    #[test]
    fn dead_zone_with_lo_above_hi_thresholds_at_hi() {
        let axial = DeadZone {
            mode: DeadZoneMode::Axial,
            lo: 0.8,
            hi: 0.2,
        };
        assert_eq!(axial.apply(Vec3::new(0.1, -0.19, 0.0)), Vec3::ZERO);
        assert_eq!(axial.apply(Vec3::new(-0.2, 0.5, 0.0)), Vec3::new(-0.2, 0.5, 0.0));
        assert_eq!(axial.apply(Vec3::new(0.5, 0.0, -0.3)), Vec3::new(0.5, 0.0, -0.3));

        let radial = DeadZone {
            mode: DeadZoneMode::Radial,
            lo: 0.9,
            hi: 0.4,
        };
        assert_eq!(radial.apply(Vec3::new(0.3, 0.0, 0.0)), Vec3::ZERO);
        assert_eq!(radial.apply(Vec3::new(0.4, 0.0, 0.0)), Vec3::new(0.4, 0.0, 0.0));
        assert_eq!(radial.apply(Vec3::new(0.3, 0.4, 0.0)), Vec3::new(0.3, 0.4, 0.0));
        assert_eq!(radial.apply(Vec3::ZERO), Vec3::ZERO);
    }

    #[test]
    fn scale_multiplies_components() {
        assert_eq!(
            apply_pure(
                &Modifier::Scale(Vec3::new(2.0, 0.5, 1.0)),
                Vec3::new(3.0, -2.0, 3.0)
            ),
            Vec3::new(6.0, -1.0, 3.0)
        );
    }

    #[test]
    fn modifiers_apply_in_order() {
        let mut smoothed = BTreeMap::new();
        let mut frame = ModifierFrame {
            delta_secs: 0.0,
            binding: BindingKey::default(),
            smoothed: &mut smoothed,
        };
        let first = apply_modifiers(
            &[
                Modifier::from(DeadZone::radial(0.2, 1.0)),
                Modifier::from(Vec3::new(2.0, 2.0, 0.0)),
            ],
            Vec3::new(0.4, 0.0, 0.0),
            &mut frame,
        );
        // Inside the dead zone's span in both orders (above `hi` both would saturate to 1):
        // dead zone then scale is 0.25 * 2, scale then dead zone is (0.8 - 0.2) / 0.8.
        assert!((first.x - 0.5).abs() < 1e-6, "{first:?}");
        let second = apply_modifiers(
            &[
                Modifier::from(Vec3::new(2.0, 2.0, 0.0)),
                Modifier::from(DeadZone::radial(0.2, 1.0)),
            ],
            Vec3::new(0.4, 0.0, 0.0),
            &mut frame,
        );
        assert!((second.x - 0.75).abs() < 1e-6, "{second:?}");
    }

    #[test]
    fn negate_swizzle_invert_y_and_curve_transform_values() {
        let negate = Modifier::Negate {
            x: true,
            y: false,
            z: false,
        };
        assert_eq!(
            apply_pure(&negate, Vec3::new(1.0, 2.0, 3.0)),
            Vec3::new(-1.0, 2.0, 3.0)
        );
        assert_eq!(
            Swizzle::Yxz.apply(Vec3::new(1.0, 2.0, 3.0)),
            Vec3::new(2.0, 1.0, 3.0)
        );
        assert_eq!(
            Swizzle::Zxy.apply(Vec3::new(1.0, 2.0, 3.0)),
            Vec3::new(3.0, 1.0, 2.0)
        );
        assert_eq!(
            Swizzle::Xzy.apply(Vec3::new(1.0, 2.0, 3.0)),
            Vec3::new(1.0, 3.0, 2.0)
        );
        assert_eq!(
            apply_pure(&Modifier::InvertY(true), Vec3::new(1.0, 2.0, 0.0)),
            Vec3::new(1.0, -2.0, 0.0)
        );
        assert_eq!(ResponseCurve::Square.apply(0.5), 0.25);
        assert_eq!(ResponseCurve::Square.apply(-0.5), -0.25, "sign preserved");
        assert_eq!(ResponseCurve::SmoothStep.apply(1.0), 1.0);
        assert_eq!(ResponseCurve::SquareRoot.apply(0.25), 0.5);
        assert_eq!(
            apply_pure(
                &Modifier::ResponseCurve(ResponseCurve::Cubic),
                Vec3::new(0.5, 0.0, 0.0)
            ),
            Vec3::new(0.125, 0.0, 0.0)
        );
    }

    /// A zero delta with history holds the smoothed sample. The no-step frame fallback
    /// evaluates with dt = 0; snapping to the raw value there flickers look smoothing.
    #[test]
    fn audfix_smooth_holds_when_delta_is_zero() {
        let mut state = BTreeMap::new();
        let mut frame = ModifierFrame {
            delta_secs: 1.0 / 60.0,
            binding: BindingKey::default(),
            smoothed: &mut state,
        };
        assert_eq!(Modifier::Smooth(10.0).apply(Vec3::X, &mut frame), Vec3::X);
        frame.delta_secs = 0.0;
        let held = Modifier::Smooth(10.0).apply(Vec3::ZERO, &mut frame);
        assert_eq!(held, Vec3::X, "zero delta holds the smoothed value, got {held:?}");
    }

    #[test]
    fn smooth_converges_over_time_and_scales_by_delta() {
        let mut smoothed = BTreeMap::new();
        let rate = Modifier::Smooth(10.0);
        // First frame: no previous value, so the raw value passes through and becomes the state.
        let mut first = ModifierFrame {
            delta_secs: 1.0,
            binding: BindingKey::default(),
            smoothed: &mut smoothed,
        };
        assert_eq!(
            rate.apply(Vec3::new(0.5, 0.0, 0.0), &mut first),
            Vec3::new(0.5, 0.0, 0.0)
        );
        // Second frame moves the target: blend is 1-e^(-rate*dt), so the value moves that
        // fraction of the remaining distance.
        let mut second = ModifierFrame {
            delta_secs: 1.0,
            binding: BindingKey::default(),
            smoothed: &mut smoothed,
        };
        let next = rate.apply(Vec3::X, &mut second);
        let blend = 1.0 - (-10.0f32).exp();
        assert!((next.x - (0.5 + 0.5 * blend)).abs() < 1e-5, "{next:?}");
        // Rate 0 disables smoothing entirely.
        let mut disabled = ModifierFrame {
            delta_secs: 1.0,
            binding: BindingKey::default(),
            smoothed: &mut BTreeMap::new(),
        };
        assert_eq!(
            Modifier::Smooth(0.0).apply(Vec3::new(9.0, 0.0, 0.0), &mut disabled),
            Vec3::new(9.0, 0.0, 0.0)
        );

        let by_delta = Modifier::ScaleByDelta;
        let mut small = ModifierFrame {
            delta_secs: 0.016,
            binding: BindingKey::default(),
            smoothed: &mut BTreeMap::new(),
        };
        assert_eq!(
            by_delta.apply(Vec3::new(100.0, 0.0, 0.0), &mut small),
            Vec3::new(1.6, 0.0, 0.0)
        );
    }

    #[test]
    fn keys2d_axis_value_and_normalisation() {
        let source = InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD");
        let (mouse, pads, empty_keyboard) = (
            Mouse::default(),
            crate::gamepad::Gamepads::default(),
            Keyboard::default(),
        );
        let held_keyboard = keyboard(&["KeyW"]);
        let devices = Devices::new(&held_keyboard, &mouse, &pads);
        assert_eq!(source.raw(ActionKind::Axis2D, &devices), Vec3::new(0.0, 1.0, 0.0));

        let diagonal_keyboard = keyboard(&["KeyW", "KeyD"]);
        let diagonal = Devices::new(&diagonal_keyboard, &mouse, &pads);
        let moved = source.raw(ActionKind::Axis2D, &diagonal);
        assert!((moved.x - 0.5f32.sqrt()).abs() < 1e-6 && (moved.y - 0.5f32.sqrt()).abs() < 1e-6);
        assert!((moved.length() - 1.0).abs() < 1e-6, "diagonals are length 1");

        let left_keyboard = keyboard(&["KeyA"]);
        let left_only = Devices::new(&left_keyboard, &mouse, &pads);
        assert_eq!(
            source.raw(ActionKind::Axis1D, &left_only),
            Vec3::new(-1.0, 0.0, 0.0)
        );
        let right_keyboard = keyboard(&["KeyD"]);
        let right_only = Devices::new(&right_keyboard, &mouse, &pads);
        assert_eq!(
            source.raw(ActionKind::Button, &right_only),
            Vec3::new(1.0, 0.0, 0.0)
        );

        let empty = Devices::new(&empty_keyboard, &mouse, &pads);
        assert_eq!(source.raw(ActionKind::Axis2D, &empty), Vec3::ZERO);
    }

    #[test]
    fn keys2d_diagonal_is_unit_length_for_axis3d() {
        let source = InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD");
        let (mouse, pads) = (Mouse::default(), crate::gamepad::Gamepads::default());
        let keyboard = keyboard(&["KeyW", "KeyD"]);
        let devices = Devices::new(&keyboard, &mouse, &pads);
        let moved = source.raw(ActionKind::Axis3D, &devices);
        assert!((moved.length() - 1.0).abs() < 1e-6, "{moved:?}");
        assert_eq!(moved.z, 0.0);
    }

    #[test]
    fn key_and_mouse_button_sources_are_binary() {
        let space = InputSource::key("Space");
        let (mouse, pads) = (Mouse::default(), crate::gamepad::Gamepads::default());
        let empty_keyboard = Keyboard::default();
        let empty = Devices::new(&empty_keyboard, &mouse, &pads);
        for kind in [
            ActionKind::Button,
            ActionKind::Axis1D,
            ActionKind::Axis2D,
            ActionKind::Axis3D,
        ] {
            assert_eq!(space.raw(kind, &empty), Vec3::ZERO);
        }
        let held_keyboard = keyboard(&["Space"]);
        let held = Devices::new(&held_keyboard, &mouse, &pads);
        for kind in [
            ActionKind::Button,
            ActionKind::Axis1D,
            ActionKind::Axis2D,
            ActionKind::Axis3D,
        ] {
            assert_eq!(space.raw(kind, &held), Vec3::new(1.0, 0.0, 0.0));
        }
    }

    #[test]
    fn mouse_delta_passes_through_per_kind() {
        let source = InputSource::MouseDelta;
        let (pads, empty_keyboard) = (crate::gamepad::Gamepads::default(), Keyboard::default());
        let mouse = Mouse {
            delta: Vec2::new(3.0, -1.0),
            ..Mouse::default()
        };
        let devices = Devices::new(&empty_keyboard, &mouse, &pads);
        assert_eq!(
            source.raw(ActionKind::Axis2D, &devices),
            Vec3::new(3.0, -1.0, 0.0)
        );
        assert_eq!(source.raw(ActionKind::Axis1D, &devices), Vec3::new(3.0, 0.0, 0.0));
        assert_eq!(source.raw(ActionKind::Button, &devices), Vec3::X);
        let still_mouse = Mouse::default();
        let still = Devices::new(&empty_keyboard, &still_mouse, &pads);
        assert_eq!(source.raw(ActionKind::Button, &still), Vec3::ZERO);
    }

    #[test]
    fn config_builder_declares_actions_contexts_and_bindings() {
        let config = InputConfig::new()
            .action("Jump", ActionKind::Button)
            .action("Move", ActionKind::Axis2D)
            .binding(Binding::new("Jump", InputSource::key("Space")))
            .context("Menu", InputContext::consuming(10))
            .binding_in("Menu", Binding::new("MenuBack", InputSource::key("Escape")));
        assert_eq!(config.kind("Jump"), Some(ActionKind::Button));
        assert_eq!(config.kind("Move"), Some(ActionKind::Axis2D));
        assert_eq!(config.kind("Fire"), None);
        assert_eq!(config.gameplay().len(), 1);
        assert_eq!(config.gameplay()[0].action, "Jump");
        let menu = &config.contexts["Menu"];
        assert!(menu.consume_lower && menu.priority == 10 && menu.bindings.len() == 1);
    }

    #[test]
    fn ron_round_trip() {
        let ron_text = r#"
InputConfig(
    actions: {
        "Move": Axis2D,
        "Jump": Button,
    },
    contexts: {
        "Gameplay": (
            priority: 0,
            consume_lower: false,
            bindings: [
                (action: "Move", input: Keys2D(up: "KeyW", down: "KeyS", left: "KeyA", right: "KeyD")),
                (action: "Jump", input: Key("Space"), triggers: [Pressed]),
                (action: "Look", input: GamepadStick(Right), modifiers: [DeadZone((mode: Radial, lo: 0.1, hi: 1.0)), Scale((1.0, 1.0, 0.0))]),
            ],
        ),
    },
)
"#;
        let config = InputConfig::from_ron(ron_text).expect("parses");
        assert_eq!(config.kind("Move"), Some(ActionKind::Axis2D));
        assert_eq!(config.gameplay().len(), 3);
        assert_eq!(config.gameplay()[1].triggers, vec![Trigger::Pressed]);
        assert_eq!(config.gameplay()[2].modifiers.len(), 2);
        // Round trip: text → config → text → config is stable.
        let reparsed = InputConfig::from_ron(&config.to_ron()).expect("serializes");
        assert_eq!(config, reparsed);
    }

    #[test]
    fn try_to_ron_matches_to_ron_and_round_trips() {
        let config = InputConfig::new()
            .action("Move", ActionKind::Axis2D)
            .action("Jump", ActionKind::Button)
            .binding(Binding::new(
                "Move",
                InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
            ))
            .binding(Binding::new("Jump", InputSource::key("Space")));
        let ok = config.try_to_ron().expect("serializes");
        assert_eq!(ok, config.to_ron());
        // The error-surfacing variant is lossless: text → config → text is stable.
        assert_eq!(InputConfig::from_ron(&ok).expect("parses"), config);
    }

    #[test]
    fn merge_overrides_replaces_action_bindings() {
        let mut config = InputConfig::new()
            .action("Jump", ActionKind::Button)
            .binding(Binding::new("Jump", InputSource::key("Space")))
            .binding(Binding::new(
                "Jump",
                InputSource::gamepad_button(regia_platform::GamepadButton::South),
            ));
        let mut overrides = crate::rebind::InputOverrides::default();
        overrides.set(
            GAMEPLAY_CONTEXT,
            "Jump",
            Binding::new("Jump", InputSource::key("KeyJ")),
        );
        let applied = config.merge_overrides(&overrides);
        assert_eq!(applied, 1);
        let bindings = config.gameplay();
        assert_eq!(bindings.len(), 1, "both old bindings replaced: {bindings:?}");
        assert_eq!(bindings[0].input, InputSource::key("KeyJ"));
    }

    #[test]
    fn spacemouse_section_parses_and_stays_out_of_default_ron() {
        let config = InputConfig::from_ron(
            "(spacemouse: (enabled: false, sensitivity: 0.01, invert: (true, false, false, false, true, false)))",
        )
        .expect("parses");
        assert!(!config.spacemouse.enabled);
        assert_eq!(config.spacemouse.invert, [true, false, false, false, true, false]);
        assert_eq!(
            config.spacemouse.sanitized().sensitivity,
            SpaceMouseConfig::SENSITIVITY_MIN
        );
        assert_eq!(
            InputConfig::from_ron(&config.to_ron()).expect("round-trips"),
            config
        );
        assert!(
            !InputConfig::new().to_ron().contains("spacemouse"),
            "defaults are omitted"
        );
        assert_eq!(
            InputConfig::from_ron("()").expect("parses").spacemouse,
            SpaceMouseConfig::DEFAULT
        );
    }

    #[test]
    fn non_finite_sensitivity_becomes_one_and_bounds_stay_put() {
        let kept = SpaceMouseConfig {
            enabled: false,
            invert: [true, false, false, false, false, true],
            ..SpaceMouseConfig::DEFAULT
        };
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let sanitized = SpaceMouseConfig {
                sensitivity: bad,
                ..kept
            }
            .sanitized();
            assert_eq!(sanitized.sensitivity, 1.0);
            assert!(!sanitized.enabled);
            assert_eq!(sanitized.invert, kept.invert);
        }
        assert_eq!(
            SpaceMouseConfig {
                sensitivity: 0.0,
                ..SpaceMouseConfig::DEFAULT
            }
            .sanitized()
            .sensitivity,
            SpaceMouseConfig::SENSITIVITY_MIN
        );
        assert_eq!(
            SpaceMouseConfig {
                sensitivity: SpaceMouseConfig::SENSITIVITY_MIN,
                ..SpaceMouseConfig::DEFAULT
            }
            .sanitized()
            .sensitivity,
            SpaceMouseConfig::SENSITIVITY_MIN
        );
        assert_eq!(
            SpaceMouseConfig {
                sensitivity: SpaceMouseConfig::SENSITIVITY_MAX,
                ..SpaceMouseConfig::DEFAULT
            }
            .sanitized()
            .sensitivity,
            SpaceMouseConfig::SENSITIVITY_MAX
        );
        assert_eq!(
            SpaceMouseConfig {
                sensitivity: 100.0,
                ..SpaceMouseConfig::DEFAULT
            }
            .sanitized()
            .sensitivity,
            SpaceMouseConfig::SENSITIVITY_MAX
        );
        assert!(SpaceMouseConfig::DEFAULT.is_default());
        assert!(SpaceMouseConfig::default().sanitized().is_default());
        assert!(!kept.is_default());
        assert!(
            !SpaceMouseConfig {
                sensitivity: 2.0,
                ..SpaceMouseConfig::DEFAULT
            }
            .is_default()
        );
    }
}
