// SPDX-License-Identifier: MPL-2.0
//! Device simulation for the separate play window (spec addendum § 15.3, ADR-7829).
//!
//! A **device preset** gives the play window a device's screen: its resolution, its safe-area
//! insets (the UI designer's own device data, `crate::ui_designer_preview::SIZE_PRESETS`, so the
//! designer and play agree) and a display cutout derived from the inset on the cutout's edge. The
//! play world gets the insets as `regia_platform::SafeArea`, as a phone build reads them from the
//! OS, so the game's UI lays out around the cutout. **Orientation** turns the device a quarter turn
//! (the cutout edge goes to the left in landscape).
//!
//! **Touch emulation** turns the mouse into a finger: a left press, drag and release become one
//! touch (`regia_platform::TouchInput`, id 0) instead of mouse input; with Ctrl held at the press a
//! second touch (id 1) mirrors the first around the press point, so dragging away from it spreads
//! two fingers (pinch out) and dragging back closes them.
//!
//! The **virtual gamepad** is a standard pad laid out over the picture (a direction pad bottom
//! left, four face buttons bottom right, Select and Start bottom center). A left press on one of
//! its buttons presses that button on a virtual pad (`regia_platform::GamepadButtonChanged`,
//! [`VIRTUAL_PAD_DEVICE`]) until the release; the pad announces itself with `GamepadConnected` on
//! its first press. A press anywhere else goes on as touch or mouse input.
//!
//! The settings are [`DeviceSim`] in the edit world (view state, never saved with the level):
//! `editor.play { window: true, device: { preset, landscape, touch, gamepad, safe_area_outline } }`
//! sets them for a session. `editor.play_controls.device` changes the outline or queues a pad
//! transition without restarting play. `editor.play_state.device` reports screen geometry, pad
//! rectangles in play pixels, and held/focused buttons. The presentation overlay draws only onto
//! the composed window; Tab and Space/Enter operate its pad while gameplay input is uncaptured.

use regia_platform::GamepadButton;

use crate::engine::ecs::prelude::*;

/// A device the play window can simulate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevicePreset {
    /// Protocol id (`phone`).
    pub id: &'static str,
    /// Menu text.
    pub label: &'static str,
    /// The UI designer preset its screen and insets come from (`SIZE_PRESETS` name).
    pub designer: &'static str,
}

/// The device presets, in menu order. Each names the UI designer size preset it shares.
pub const DEVICE_PRESETS: &[DevicePreset] = &[
    DevicePreset {
        id: "phone",
        label: "Phone (cutout, 20:9)",
        designer: "1080x2400",
    },
    DevicePreset {
        id: "handheld",
        label: "Handheld (16:10)",
        designer: "1280x800",
    },
];

/// The virtual pad's device id (far from the ids real pads get).
pub const VIRTUAL_PAD_DEVICE: u64 = 0x5649_5254_5041_4400;

/// An unknown `device.preset` (`editor.play`).
pub const DEVICE_PRESET_UNKNOWN: &str = "play_device_preset_unknown";
/// A `device` without `window: true`.
pub const DEVICE_NEEDS_WINDOW: &str = "play_device_needs_window";

/// The play window's device simulation (edit-world view state).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceSim {
    /// The simulated device, when one is chosen.
    pub preset: Option<&'static DevicePreset>,
    /// Turned a quarter turn (cutout edge on the left).
    pub landscape: bool,
    /// Mouse drags become touches.
    pub touch: bool,
    /// The virtual gamepad is over the picture.
    pub gamepad: bool,
    /// Draw the safe-area outline over the picture (view state only).
    pub safe_area_outline: bool,
}

impl DeviceSim {
    /// The device's screen in its orientation, when a preset is chosen.
    #[must_use]
    pub fn layout(&self) -> Option<DeviceLayout> {
        self.preset
            .and_then(|preset| DeviceLayout::of(preset, self.landscape))
    }
}

/// The preset called `id`.
#[must_use]
pub fn preset(id: &str) -> Option<&'static DevicePreset> {
    DEVICE_PRESETS.iter().find(|preset| preset.id == id)
}

/// A device screen in one orientation, physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceLayout {
    /// Screen size.
    pub size: [u32; 2],
    /// Safe-area insets: left, top, right, bottom.
    pub safe_area: [u32; 4],
    /// The display cutout `[x, y, width, height]` (zero size when the device has none).
    pub cutout: [u32; 4],
}

impl DeviceLayout {
    /// `preset`'s screen, portrait as the designer gives it or turned to `landscape`.
    #[must_use]
    pub fn of(preset: &DevicePreset, landscape: bool) -> Option<Self> {
        let (_, size, insets) = crate::ui_designer_preview::SIZE_PRESETS
            .iter()
            .find(|(name, _, _)| *name == preset.designer)?;
        // The designer's sizes and insets are whole, small pixel counts.
        let px = |v: f32| v.max(0.0).round() as u32;
        let mut size = [px(size[0]), px(size[1])];
        let mut safe = [px(insets[0]), px(insets[1]), px(insets[2]), px(insets[3])];
        // The designer's portrait/landscape shape is the device's natural one; `landscape` asks for
        // the wide one.
        let wide = size[0] > size[1];
        if wide != landscape {
            size = [size[1], size[0]];
            safe = if landscape {
                // Portrait → landscape, turned left: top → left, right → top, bottom → right,
                // left → bottom.
                [safe[1], safe[2], safe[3], safe[0]]
            } else {
                // Landscape → portrait: the inverse turn.
                [safe[3], safe[0], safe[1], safe[2]]
            };
        }
        let cutout = cutout_of(size, safe);
        Some(Self {
            size,
            safe_area: safe,
            cutout,
        })
    }

    /// The safe area as the play world reads it.
    #[must_use]
    pub fn safe_area(&self) -> regia_platform::SafeArea {
        let [left, top, right, bottom] = self.safe_area;
        regia_platform::SafeArea {
            left,
            top,
            right,
            bottom,
        }
    }
}

/// The cutout of a screen whose deepest inset is the cutout's edge: centered on that edge, a third
/// of the edge long and 60 % of the inset deep. No inset, no cutout.
fn cutout_of(size: [u32; 2], safe: [u32; 4]) -> [u32; 4] {
    let (edge, depth) = safe
        .iter()
        .enumerate()
        .max_by_key(|(_, inset)| **inset)
        .map_or((0, 0), |(edge, inset)| (edge, *inset));
    if depth == 0 {
        return [0; 4];
    }
    let deep = depth * 3 / 5;
    let [w, h] = size;
    match edge {
        // Left or right edge: a vertical cutout.
        0 | 2 => {
            let long = h / 3;
            let x = if edge == 0 { 0 } else { w - deep };
            [x, (h - long) / 2, deep, long]
        }
        // Top or bottom edge.
        _ => {
            let long = w / 3;
            let y = if edge == 1 { 0 } else { h - deep };
            [(w - long) / 2, y, long, deep]
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Touch emulation.

/// One emulated touch event: finger id, phase, position in play pixels.
pub type TouchEvent = (u64, regia_platform::TouchPhase, (f64, f64));

/// Turns left presses, drags and releases into touches (see the module docs).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TouchEmulator {
    /// The press point while a touch is down.
    anchor: Option<(f64, f64)>,
    /// A second, mirrored finger is down.
    pinch: bool,
    /// The last primary finger position.
    last: (f64, f64),
}

impl TouchEmulator {
    /// The mirror of `at` around the press point.
    fn mirror(anchor: (f64, f64), at: (f64, f64)) -> (f64, f64) {
        (2.0 * anchor.0 - at.0, 2.0 * anchor.1 - at.1)
    }

    /// A left press at `at`; `pinch` (Ctrl held) adds the mirrored second finger.
    pub fn press(&mut self, at: (f64, f64), pinch: bool) -> Vec<TouchEvent> {
        use regia_platform::TouchPhase::Started;
        let mut out = self.cancel();
        self.anchor = Some(at);
        self.pinch = pinch;
        self.last = at;
        out.push((0, Started, at));
        if pinch {
            out.push((1, Started, at));
        }
        out
    }

    /// The cursor moved to `at` (nothing while no touch is down).
    pub fn moved(&mut self, at: (f64, f64)) -> Vec<TouchEvent> {
        use regia_platform::TouchPhase::Moved;
        let Some(anchor) = self.anchor else {
            return Vec::new();
        };
        self.last = at;
        let mut out = vec![(0, Moved, at)];
        if self.pinch {
            out.push((1, Moved, Self::mirror(anchor, at)));
        }
        out
    }

    /// The left button came up.
    pub fn release(&mut self) -> Vec<TouchEvent> {
        self.end(regia_platform::TouchPhase::Ended)
    }

    /// The window lost focus or the emulation turned off with a touch down.
    pub fn cancel(&mut self) -> Vec<TouchEvent> {
        self.end(regia_platform::TouchPhase::Cancelled)
    }

    fn end(&mut self, phase: regia_platform::TouchPhase) -> Vec<TouchEvent> {
        let Some(anchor) = self.anchor.take() else {
            return Vec::new();
        };
        let mut out = vec![(0, phase, self.last)];
        if std::mem::take(&mut self.pinch) {
            out.push((1, phase, Self::mirror(anchor, self.last)));
        }
        out
    }

    /// Whether a touch is down.
    #[must_use]
    pub fn active(&self) -> bool {
        self.anchor.is_some()
    }
}

// ---------------------------------------------------------------------------------------------
// Virtual gamepad.

/// The virtual pad's buttons with their rects, normalized to the picture (`[x, y, w, h]`, 0..1).
pub const PAD_BUTTONS: [(GamepadButton, &str, [f32; 4]); 10] = [
    (GamepadButton::DPadUp, "dpad_up", [0.08, 0.62, 0.08, 0.10]),
    (GamepadButton::DPadDown, "dpad_down", [0.08, 0.82, 0.08, 0.10]),
    (GamepadButton::DPadLeft, "dpad_left", [0.00, 0.72, 0.08, 0.10]),
    (GamepadButton::DPadRight, "dpad_right", [0.16, 0.72, 0.08, 0.10]),
    (GamepadButton::North, "north", [0.84, 0.62, 0.08, 0.10]),
    (GamepadButton::South, "south", [0.84, 0.82, 0.08, 0.10]),
    (GamepadButton::West, "west", [0.76, 0.72, 0.08, 0.10]),
    (GamepadButton::East, "east", [0.92, 0.72, 0.08, 0.10]),
    (GamepadButton::Select, "select", [0.40, 0.88, 0.08, 0.07]),
    (GamepadButton::Start, "start", [0.52, 0.88, 0.08, 0.07]),
];

/// The virtual pad's button rects in play pixels for a `size` picture: `(name, [x, y, w, h])`.
#[must_use]
pub fn pad_rects(size: [u32; 2]) -> Vec<(&'static str, [f64; 4])> {
    pad_rects_scaled(size, 1.0)
}

/// Zoom the existing button clusters about their outside/bottom anchors. At larger
/// zoom the corner clusters move up to leave the center buttons a separate strip.
/// Fit at most twice the base size so the left and right clusters never overlap.
pub(crate) fn pad_rects_scaled(size: [u32; 2], scale: f32) -> Vec<(&'static str, [f64; 4])> {
    let [w, h] = [f64::from(size[0]), f64::from(size[1])];
    let scale = f64::from(scale.clamp(0.5, 2.0));
    PAD_BUTTONS
        .iter()
        .enumerate()
        .map(|(i, (_, name, [x, y, bw, bh]))| {
            let (ax, ay) = if i < 4 {
                (0.0, 0.92)
            } else if i < 8 {
                (1.0, 0.92)
            } else {
                (0.5, 0.95)
            };
            let lift = if i < 8 { (scale - 1.0).max(0.0) * 0.2 } else { 0.0 };
            (
                *name,
                [
                    (ax + (f64::from(*x) - ax) * scale) * w,
                    (ay - lift + (f64::from(*y) - ay) * scale) * h,
                    f64::from(*bw) * scale * w,
                    f64::from(*bh) * scale * h,
                ],
            )
        })
        .collect()
}

pub(crate) fn ui_scale(world: &World) -> f32 {
    world
        .get_resource::<crate::editor_prefs::EditorPreferences>()
        .map_or(1.0, |prefs| prefs.float("appearance.ui_scale") as f32)
}

/// The pad button under `at` (play pixels) on a `size` picture at the default UI scale.
#[must_use]
pub fn pad_button_at(at: (f64, f64), size: [u32; 2]) -> Option<GamepadButton> {
    pad_button_at_scaled(at, size, 1.0)
}

pub(crate) fn pad_button_at_scaled(at: (f64, f64), size: [u32; 2], scale: f32) -> Option<GamepadButton> {
    PAD_BUTTONS
        .iter()
        .zip(pad_rects_scaled(size, scale))
        .find_map(|((button, _, _), (_, [x, y, w, h]))| {
            (at.0 >= x && at.0 < x + w && at.1 >= y && at.1 < y + h).then_some(*button)
        })
}

/// The virtual pad's press state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VirtualPad {
    /// The pad announced itself to the play world.
    connected: bool,
    /// The button held by the mouse.
    held: Option<GamepadButton>,
}

impl VirtualPad {
    /// A left press at `at`: the button pressed (and whether the pad must announce itself first),
    /// or `None` when the press is not on a button.
    pub fn press(&mut self, at: (f64, f64), size: [u32; 2]) -> Option<(GamepadButton, bool)> {
        let button = pad_button_at(at, size)?;
        Some(self.press_button(button))
    }

    /// Presses a named button (shared by keyboard, pointer and agent input).
    pub fn press_button(&mut self, button: GamepadButton) -> (GamepadButton, bool) {
        let announce = !std::mem::replace(&mut self.connected, true);
        self.held = Some(button);
        (button, announce)
    }

    /// The left button came up: the pad button to release, when one is held.
    pub fn release(&mut self) -> Option<GamepadButton> {
        self.held.take()
    }

    /// The button currently held, for the presentation overlay.
    #[must_use]
    pub fn held(&self) -> Option<GamepadButton> {
        self.held
    }

    /// Whether a pad button is held.
    #[must_use]
    pub fn holding(&self) -> bool {
        self.held.is_some()
    }

    /// Forgets the announcement (a new play world needs a new `GamepadConnected`).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

// ---------------------------------------------------------------------------------------------
// Agent surface: `editor.play { device }` and `editor.play_state.device`.

/// Parses `editor.play`'s `device: { preset, landscape, touch, gamepad }` (`null` turns the device
/// simulation off). `window` is whether the request opens the play window.
///
/// # Errors
///
/// [`DEVICE_PRESET_UNKNOWN`], [`DEVICE_NEEDS_WINDOW`], or a `device` that is not an object.
pub fn device_param(
    params: &serde_json::Value,
    window: bool,
) -> Result<Option<DeviceSim>, regia_agent::protocol::RpcError> {
    use crate::agent_ext::play_option_error;
    let Some(device) = params.get("device") else {
        return Ok(None);
    };
    if device.is_null() {
        return Ok(Some(DeviceSim::default()));
    }
    let Some(object) = device.as_object() else {
        return Err(regia_agent::protocol::RpcError::invalid_params(
            "`device` must be an object { preset, landscape, touch, gamepad } or null",
        ));
    };
    if !window {
        return Err(play_option_error(
            DEVICE_NEEDS_WINDOW,
            "`device` simulates a device in the play window: pass `window: true` too",
        ));
    }
    let flag = |key: &str| {
        object
            .get(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    let preset = match object.get("preset").filter(|v| !v.is_null()) {
        None => None,
        Some(id) => Some(preset(id.as_str().unwrap_or("")).ok_or_else(|| {
            let ids: Vec<&str> = DEVICE_PRESETS.iter().map(|p| p.id).collect();
            play_option_error(
                DEVICE_PRESET_UNKNOWN,
                format!("unknown device preset {id}; presets: {}", ids.join(", ")),
            )
        })?),
    };
    Ok(Some(DeviceSim {
        preset,
        landscape: flag("landscape"),
        touch: flag("touch"),
        gamepad: flag("gamepad"),
        safe_area_outline: flag("safe_area_outline"),
    }))
}

/// `editor.play_state.device`: the settings and, with a preset, the screen, insets, cutout and the
/// pad's button rects in play pixels.
#[must_use]
pub fn device_json(sim: &DeviceSim) -> serde_json::Value {
    let layout = sim.layout();
    serde_json::json!({
        "preset": sim.preset.map(|p| p.id),
        "landscape": sim.landscape,
        "touch": sim.touch,
        "gamepad": sim.gamepad,
        "safe_area_outline": sim.safe_area_outline,
        "size": layout.map(|l| l.size),
        "safe_area": layout.map(|l| l.safe_area),
        "cutout": layout.map(|l| l.cutout),
        "pad_buttons": if sim.gamepad {
            layout.map_or_else(Vec::new, |l| {
                pad_rects(l.size)
                    .into_iter()
                    .map(|(name, rect)| serde_json::json!({ "button": name, "rect": rect }))
                    .collect()
            })
        } else {
            Vec::new()
        },
    })
}

// ---------------------------------------------------------------------------------------------
// The play toolbar's device menu.

/// The play toolbar's device menu (where the host has a play window): no device or a preset, the
/// orientation, touch emulation and the virtual gamepad. Changes apply to an open play window at
/// once. Every entry is a focusable button or toggle (keyboard: Tab, Space).
pub fn device_menu(ui: &mut egui::Ui, world: &mut World) {
    use crate::agent_parity::{
        PLAY_DEVICE, PLAY_DEVICE_GAMEPAD, PLAY_DEVICE_ROTATE, PLAY_DEVICE_SAFE_AREA, PLAY_DEVICE_TOUCH,
        action_button_text, action_toggle,
    };
    let mut sim = world.get_resource::<DeviceSim>().copied().unwrap_or_default();
    let label = sim.preset.map_or("No device", |preset| preset.label);
    let before = sim;
    ui.menu_button(format!("📱 {label}"), |ui| {
        let mark = |on: bool| if on { "● " } else { "○ " };
        if action_button_text(
            ui,
            &PLAY_DEVICE,
            format!("{}No device", mark(sim.preset.is_none())),
        )
        .clicked()
        {
            sim.preset = None;
        }
        for preset in DEVICE_PRESETS {
            let on = sim.preset.is_some_and(|p| p.id == preset.id);
            if action_button_text(ui, &PLAY_DEVICE, format!("{}{}", mark(on), preset.label)).clicked() {
                sim.preset = Some(preset);
            }
        }
        ui.separator();
        let turn = if sim.landscape {
            "⟲ Turn to portrait"
        } else {
            "⟳ Turn to landscape"
        };
        if action_button_text(ui, &PLAY_DEVICE_ROTATE, turn).clicked() {
            sim.landscape = !sim.landscape;
        }
        let _ = action_toggle(ui, &PLAY_DEVICE_TOUCH, &mut sim.touch)
            .on_hover_text("Left drags become touches; hold Ctrl at the press for a two-finger pinch");
        let _ = action_toggle(ui, &PLAY_DEVICE_GAMEPAD, &mut sim.gamepad)
            .on_hover_text("Click a pad button, or use Tab and Space in the uncaptured play window");
        let _ = action_toggle(ui, &PLAY_DEVICE_SAFE_AREA, &mut sim.safe_area_outline);
    });
    if sim != before && menu_first_in_frame(world, ui.ctx().cumulative_frame_nr()) {
        world.insert_resource(sim);
    }
}

/// Last frame in which the device menu changed state (egui may run two passes).
#[derive(Resource, Default)]
struct DeviceMenuFrame(Option<u64>);

fn menu_first_in_frame(world: &mut World, frame: u64) -> bool {
    let mut seen = world.get_resource_or_insert_with(DeviceMenuFrame::default);
    if seen.0 == Some(frame) {
        return false;
    }
    seen.0 = Some(frame);
    true
}

/// Presentation input state, also reported by `editor.play_state.device`.
#[derive(Resource, Default)]
pub(crate) struct DeviceInputState {
    pub(crate) held: Option<GamepadButton>,
    pub(crate) focused: Option<usize>,
}

pub(crate) fn device_state_json(world: &World) -> serde_json::Value {
    let sim = world.get_resource::<DeviceSim>().copied().unwrap_or_default();
    let mut value = device_json(&sim);
    if sim.gamepad
        && let Some(window) = world.get_resource::<crate::play_window::PlayWindowStatus>()
    {
        value["pad_buttons"] = serde_json::json!(
            pad_rects_scaled(window.render_size, ui_scale(world))
                .into_iter()
                .map(|(name, rect)| serde_json::json!({"button": name, "rect": rect}))
                .collect::<Vec<_>>()
        );
    }
    let state = world.get_resource::<DeviceInputState>();
    value["held_button"] = serde_json::json!(state.and_then(|state| state.held).and_then(|held| {
        PAD_BUTTONS
            .iter()
            .find(|(button, _, _)| *button == held)
            .map(|(_, name, _)| name)
    }));
    value["focused_button"] = serde_json::json!(
        state
            .and_then(|state| state.focused)
            .and_then(|i| PAD_BUTTONS.get(i).map(|(_, name, _)| name))
    );
    value
}

/// A queued device control, bound to the play world that accepted it.
#[derive(Resource, Default)]
pub(crate) struct DeviceControlQueue {
    pub(crate) world: Option<regia_ecs::ecs::world::WorldId>,
    pub(crate) buttons: Vec<(GamepadButton, bool)>,
}

/// Validated live device controls (no mutation until all play controls validate).
pub(crate) struct DeviceControl {
    outline: Option<bool>,
    button: Option<(GamepadButton, bool)>,
}

pub(crate) fn control_param(
    world: &World,
    params: &serde_json::Value,
) -> Result<Option<DeviceControl>, regia_agent::protocol::RpcError> {
    use regia_agent::protocol::RpcError;
    let Some(value) = params.get("device") else {
        return Ok(None);
    };
    let invalid = || {
        RpcError::invalid_params(
            "device needs { safe_area_outline?: bool, button?: pad button name, pressed?: bool }; button and pressed must be supplied together, with an open play window and virtual gamepad enabled",
        )
    };
    let object = value.as_object().ok_or_else(invalid)?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "safe_area_outline" | "button" | "pressed"))
    {
        return Err(invalid());
    }
    if !world
        .get_resource::<crate::play_window::PlayWindowStatus>()
        .is_some_and(|s| s.open)
    {
        return Err(crate::agent_ext::play_option_error(
            DEVICE_NEEDS_WINDOW,
            "device controls need an open play window",
        ));
    }
    let outline = object
        .get("safe_area_outline")
        .map(|v| v.as_bool().ok_or_else(invalid))
        .transpose()?;
    let button = match (object.get("button"), object.get("pressed")) {
        (None, None) => None,
        (Some(name), Some(pressed)) => {
            let button = PAD_BUTTONS
                .iter()
                .find(|(_, id, _)| Some(*id) == name.as_str())
                .map(|(button, _, _)| *button)
                .ok_or_else(invalid)?;
            let pressed = pressed.as_bool().ok_or_else(invalid)?;
            if !world.get_resource::<DeviceSim>().is_some_and(|sim| sim.gamepad) {
                return Err(invalid());
            }
            Some((button, pressed))
        }
        _ => return Err(invalid()),
    };
    if button.is_some()
        && world
            .get_resource::<DeviceControlQueue>()
            .is_none_or(|queue| queue.world.is_none() || queue.buttons.len() >= 128)
    {
        return Err(invalid());
    }
    Ok(Some(DeviceControl { outline, button }))
}

pub(crate) fn apply_control(world: &mut World, control: DeviceControl) -> bool {
    if let Some(outline) = control.outline {
        world
            .get_resource_or_insert_with(DeviceSim::default)
            .safe_area_outline = outline;
    }
    if let Some(button) = control.button {
        world
            .get_resource_or_insert_with(DeviceControlQueue::default)
            .buttons
            .push(button);
    }
    control.outline.is_some() || control.button.is_some()
}

/// GPU resources for the device overlay. Reused across play-window frames and resizes.
pub struct DeviceOverlay {
    ctx: egui::Context,
    renderer: egui_wgpu::Renderer,
}

/// One presentation's overlay geometry and state, in physical window pixels.
pub(crate) struct OverlayFrame<'a> {
    pub(crate) sim: DeviceSim,
    pub(crate) picture: crate::play_window::Letterbox,
    pub(crate) size: [u32; 2],
    pub(crate) scale: f32,
    pub(crate) native_scale: f32,
    pub(crate) held: Option<GamepadButton>,
    pub(crate) focused: Option<usize>,
    pub(crate) tokens: &'a crate::theme::ThemeTokens,
}

impl DeviceOverlay {
    /// Creates an overlay for the play surface's non-sRGB view format.
    #[must_use]
    pub fn new(device: &regia_render::RenderDevice, format: wgpu::TextureFormat) -> Self {
        Self {
            ctx: egui::Context::default(),
            renderer: egui_wgpu::Renderer::new(&device.device, format, Default::default()),
        }
    }

    pub(crate) fn draw(
        &mut self,
        device: &regia_render::RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        frame: OverlayFrame<'_>,
    ) {
        if (frame.sim.preset.is_none() && !frame.sim.gamepad)
            || frame.picture.width == 0
            || frame.picture.height == 0
        {
            return;
        }
        let _span = tracing::info_span!(target: "regia_editor", "play_device_overlay").entered();
        let mut output = self.ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(frame.size[0] as f32, frame.size[1] as f32),
                )),
                ..Default::default()
            },
            |ui| paint_overlay(ui, &frame),
        );
        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                self.renderer
                    .update_texture(&device.device, &device.queue, *id, delta);
            }
        }
        let jobs = self.ctx.tessellate(output.shapes, output.pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: frame.size,
            pixels_per_point: output.pixels_per_point,
        };
        self.renderer
            .update_buffers(&device.device, &device.queue, encoder, &jobs, &screen);
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("play device overlay after picture"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            self.renderer.render(&mut pass, &jobs, &screen);
        }
        for id in &output.textures_delta.free {
            self.renderer.free_texture(id);
        }
        output.textures_delta.clear();
    }
}

fn paint_overlay(ui: &mut egui::Ui, frame: &OverlayFrame<'_>) {
    use egui::{Rect, Stroke, StrokeKind, pos2, vec2};
    let box_ = frame.picture;
    let picture = Rect::from_min_size(
        pos2(box_.x as f32, box_.y as f32),
        vec2(box_.width as f32, box_.height as f32),
    );
    let painter = ui.painter().with_clip_rect(picture);
    let scale = frame.scale.clamp(0.5, 4.0);
    let tokens = frame.tokens;
    let stroke_scale = scale * frame.native_scale;
    let rect_of = |[x, y, w, h]: [f64; 4], size: [u32; 2]| {
        let sx = picture.width() / size[0].max(1) as f32;
        let sy = picture.height() / size[1].max(1) as f32;
        Rect::from_min_size(
            picture.min + vec2(x as f32 * sx, y as f32 * sy),
            vec2(w as f32 * sx, h as f32 * sy),
        )
    };
    if let Some(layout) = frame.sim.layout() {
        let [x, y, w, h] = layout.cutout.map(f64::from);
        if w > 0.0 && h > 0.0 {
            painter.rect_filled(rect_of([x, y, w, h], layout.size), 0.0, tokens.well);
        }
        painter.rect_stroke(
            picture,
            0.0,
            Stroke::new(3.0 * stroke_scale, tokens.strong),
            StrokeKind::Inside,
        );
        if frame.sim.safe_area_outline {
            let [left, top, right, bottom] = layout.safe_area;
            let safe = rect_of(
                [
                    f64::from(left),
                    f64::from(top),
                    f64::from(layout.size[0].saturating_sub(left + right)),
                    f64::from(layout.size[1].saturating_sub(top + bottom)),
                ],
                layout.size,
            );
            painter.rect_stroke(
                safe,
                0.0,
                Stroke::new(2.0 * stroke_scale, tokens.overlay.bounds),
                StrokeKind::Inside,
            );
        }
    }
    if frame.sim.gamepad {
        let size = [box_.width, box_.height];
        for (i, ((button, name, _), (_, rect))) in
            PAD_BUTTONS.iter().zip(pad_rects_scaled(size, scale)).enumerate()
        {
            let rect = rect_of(rect, size);
            let held = frame.held == Some(*button);
            let fill = if held { tokens.signal } else { tokens.raised };
            let text = if held { tokens.on_signal } else { tokens.text };
            painter.rect_filled(rect, 2.0, fill);
            painter.rect_stroke(
                rect,
                2.0,
                Stroke::new(
                    if frame.focused == Some(i) {
                        3.0 * stroke_scale
                    } else {
                        stroke_scale
                    },
                    if frame.focused == Some(i) {
                        tokens.accents.select
                    } else {
                        tokens.line
                    },
                ),
                StrokeKind::Inside,
            );
            let label = name.replace("dpad_", "");
            painter.with_clip_rect(rect).text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(11.0 * scale.min(2.0) * frame.native_scale),
                text,
            );
        }
        crate::canvas_keys::name_canvas(
            ui.ctx(),
            egui::Id::new("play_device_pad"),
            "Virtual gamepad: Tab selects, Space or Enter holds a button, Escape leaves the pad",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_platform::TouchPhase::{Ended, Moved, Started};

    fn composed_pixels(sim: DeviceSim, held: Option<GamepadButton>, scale: f32) -> Vec<u8> {
        use crate::play_blit::LetterboxBlit;
        use crate::play_window::Letterbox;
        let device = regia_render::RenderDevice::headless().expect("lavapipe adapter");
        let gpu = &device.device;
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let size = wgpu::Extent3d {
            width: 512,
            height: 256,
            depth_or_array_layers: 1,
        };
        let texture = |label, usage| {
            gpu.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let picture = texture(
            "game picture",
            wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
        );
        let target = texture(
            "composed play window",
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        );
        let game_color = [200u8, 10, 10, 255];
        device.queue.write_texture(
            picture.as_image_copy(),
            &game_color.repeat(512 * 256),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(512 * 4),
                rows_per_image: Some(256),
            },
            size,
        );
        let size_pixels = sim.layout().map_or([512, 256], |layout| layout.size);
        let rect = Letterbox::fit(size_pixels, [512, 256]);
        let mut encoder = gpu.create_command_encoder(&Default::default());
        let target_view = target.create_view(&Default::default());
        LetterboxBlit::new(&device, format).draw(
            &device,
            &mut encoder,
            &picture.create_view(&Default::default()),
            &target_view,
            rect,
        );
        DeviceOverlay::new(&device, format).draw(
            &device,
            &mut encoder,
            &target_view,
            OverlayFrame {
                sim,
                picture: rect,
                size: [512, 256],
                scale,
                native_scale: 1.0,
                held,
                focused: None,
                tokens: &crate::theme::ThemeTokens::regia_default(),
            },
        );
        let readback = gpu.create_buffer(&wgpu::BufferDescriptor {
            label: Some("window and unchanged game pixels"),
            size: 512 * 256 * 4 * 2,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        for (i, source) in [&target, &picture].iter().enumerate() {
            encoder.copy_texture_to_buffer(
                source.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: i as u64 * 512 * 256 * 4,
                        bytes_per_row: Some(512 * 4),
                        rows_per_image: Some(256),
                    },
                },
                size,
            );
        }
        device.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        gpu.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        rx.recv().expect("callback").expect("mapped");
        let bytes = readback.slice(..).get_mapped_range().expect("pixels");
        assert!(
            bytes[512 * 256 * 4..]
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| *pixel == game_color),
            "the game's render target stays unchanged"
        );
        bytes[..512 * 256 * 4].to_vec()
    }

    /// Read the composed window, not the game's render target. No golden image is used.
    #[test]
    fn the_cutout_and_safe_area_are_drawn_over_the_picture() {
        let sim = DeviceSim {
            preset: preset("phone"),
            landscape: true,
            safe_area_outline: true,
            ..DeviceSim::default()
        };
        let bytes = composed_pixels(sim, None, 1.0);
        let game_color = [200u8, 10, 10, 255];
        let pixel = |x: usize, y: usize| &bytes[(y * 512 + x) * 4..(y * 512 + x + 1) * 4];
        assert_ne!(
            pixel(7, 128),
            game_color,
            "cutout must cover the composed picture"
        );
        assert_ne!(
            pixel(24, 64),
            game_color,
            "safe-area outline must cover the composed picture"
        );
        assert_eq!(pixel(256, 128), game_color, "uncovered game picture");
        let off = composed_pixels(
            DeviceSim {
                safe_area_outline: false,
                ..sim
            },
            None,
            1.0,
        );
        assert_eq!(
            &off[(64 * 512 + 24) * 4..(64 * 512 + 25) * 4],
            game_color,
            "the outline toggle clears only the outline"
        );
        assert_eq!(
            &off[(128 * 512 + 7) * 4..(128 * 512 + 8) * 4],
            pixel(7, 128),
            "cutout remains visible with the outline off"
        );
        let portrait = composed_pixels(
            DeviceSim {
                landscape: false,
                ..sim
            },
            None,
            1.0,
        );
        assert_ne!(
            &portrait[(3 * 512 + 256) * 4..(3 * 512 + 257) * 4],
            game_color,
            "portrait cutout covers the top edge"
        );
        assert_eq!(
            &portrait[(128 * 512 + 256) * 4..(128 * 512 + 257) * 4],
            game_color
        );
    }

    #[test]
    fn the_pad_is_visible_without_a_device_and_lights_while_held() {
        let sim = DeviceSim {
            gamepad: true,
            ..DeviceSim::default()
        };
        let idle = composed_pixels(sim, None, 1.0);
        let held = composed_pixels(sim, Some(GamepadButton::South), 1.0);
        let large = composed_pixels(sim, None, 2.0);
        let pixel =
            |bytes: &[u8], x: usize, y: usize| bytes[(y * 512 + x) * 4..(y * 512 + x + 1) * 4].to_vec();
        assert_ne!(
            pixel(&idle, 435, 215),
            vec![200, 10, 10, 255],
            "pad covers the picture"
        );
        assert_ne!(
            pixel(&idle, 435, 215),
            pixel(&held, 435, 215),
            "held button lights"
        );
        assert_ne!(idle, large, "appearance scale changes pad labels and strokes");
        assert_eq!(
            pixel(&held, 256, 128),
            vec![200, 10, 10, 255],
            "game picture remains visible"
        );
    }

    #[test]
    fn the_safe_area_toggle_has_a_keyboard_path_and_one_change_per_frame() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let ctx = egui::Context::default();
        let size = [800.0, 500.0];
        let mut world = World::new();
        let mut draw = |ui: &mut egui::Ui| device_menu(ui, &mut world);
        tab_to(&ctx, size, "📱 No device", false, 4, &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        frame(&ctx, size, vec![], &mut draw);
        tab_to(&ctx, size, "Safe-area outline", false, 12, &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Space, egui::Modifiers::NONE)],
            &mut draw,
        );
        drop(draw);
        assert!(world.resource::<DeviceSim>().safe_area_outline);
        assert!(menu_first_in_frame(&mut world, 100));
        assert!(!menu_first_in_frame(&mut world, 100));
        assert!(menu_first_in_frame(&mut world, 101));
    }

    #[test]
    fn the_phone_shares_the_designers_screen_and_insets_and_turns() {
        let phone = preset("phone").expect("phone");
        let portrait = DeviceLayout::of(phone, false).expect("designer preset");
        assert_eq!(portrait.size, [1080, 2400]);
        assert_eq!(portrait.safe_area, [0, 110, 0, 60]);
        // The cutout sits on the top edge, centered.
        assert_eq!(portrait.cutout, [360, 0, 360, 66]);
        let landscape = DeviceLayout::of(phone, true).expect("designer preset");
        assert_eq!(landscape.size, [2400, 1080]);
        // The designer's own landscape phone: cutout left, gesture side right.
        assert_eq!(landscape.safe_area, [110, 0, 60, 0]);
        assert_eq!(landscape.cutout, [0, 360, 66, 360]);
        let handheld = DeviceLayout::of(preset("handheld").expect("handheld"), true).expect("preset");
        assert_eq!(handheld.size, [1280, 800]);
        assert_eq!(handheld.cutout, [0; 4], "no inset, no cutout");
    }

    #[test]
    fn a_drag_is_one_touch_and_ctrl_adds_a_mirrored_finger() {
        let mut touch = TouchEmulator::default();
        assert!(touch.moved((5.0, 5.0)).is_empty(), "hovering is not touching");
        assert_eq!(touch.press((10.0, 10.0), false), vec![(0, Started, (10.0, 10.0))]);
        assert_eq!(touch.moved((12.0, 10.0)), vec![(0, Moved, (12.0, 10.0))]);
        assert_eq!(touch.release(), vec![(0, Ended, (12.0, 10.0))]);
        assert!(!touch.active());

        assert_eq!(touch.press((100.0, 100.0), true).len(), 2);
        assert_eq!(
            touch.moved((120.0, 100.0)),
            vec![(0, Moved, (120.0, 100.0)), (1, Moved, (80.0, 100.0))],
            "the fingers spread apart"
        );
        assert_eq!(
            touch.release(),
            vec![(0, Ended, (120.0, 100.0)), (1, Ended, (80.0, 100.0))]
        );
    }

    /// Keyboard path (accessibility bar): the device menu opens from the keyboard and Space turns
    /// touch emulation on.
    #[test]
    fn the_device_menu_works_from_the_keyboard() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let ctx = egui::Context::default();
        let size = [800.0, 400.0];
        let mut world = World::new();
        let mut draw = |ui: &mut egui::Ui| device_menu(ui, &mut world);
        tab_to(&ctx, size, "📱 No device", false, 4, &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            &mut draw,
        );
        frame(&ctx, size, vec![], &mut draw);
        tab_to(&ctx, size, "Touch emulation", false, 10, &mut draw);
        frame(
            &ctx,
            size,
            vec![key(egui::Key::Space, egui::Modifiers::NONE)],
            &mut draw,
        );
        drop(draw);
        assert!(world.get_resource::<DeviceSim>().is_some_and(|sim| sim.touch));
    }

    #[test]
    fn device_params_parse_and_refuse_by_name() {
        let reason = |e: regia_agent::protocol::RpcError| {
            e.data
                .and_then(|d| d.get("reason").and_then(|r| r.as_str().map(str::to_owned)))
                .unwrap_or_default()
        };
        let none = device_param(&serde_json::json!({}), true).expect("no device");
        assert_eq!(none, None);
        let sim = device_param(
            &serde_json::json!({ "device": { "preset": "phone", "landscape": true, "gamepad": true } }),
            true,
        )
        .expect("device")
        .expect("set");
        assert_eq!(sim.preset.map(|p| p.id), Some("phone"));
        assert!(sim.landscape && sim.gamepad && !sim.touch);
        let json = device_json(&sim);
        assert_eq!(json["size"], serde_json::json!([2400, 1080]));
        assert_eq!(
            json["pad_buttons"].as_array().map(Vec::len),
            Some(PAD_BUTTONS.len())
        );
        let off = device_param(&serde_json::json!({ "device": null }), true).expect("off");
        assert_eq!(off, Some(DeviceSim::default()));
        let unknown =
            device_param(&serde_json::json!({ "device": { "preset": "watch" } }), true).expect_err("unknown");
        assert_eq!(reason(unknown), DEVICE_PRESET_UNKNOWN);
        let windowless =
            device_param(&serde_json::json!({ "device": { "touch": true } }), false).expect_err("window");
        assert_eq!(reason(windowless), DEVICE_NEEDS_WINDOW);
    }

    #[test]
    fn scaled_pad_buttons_fit_and_hit_the_same_visible_rectangles() {
        let size = [324, 720];
        for scale in [0.5, 1.0, 1.5, 2.0, 3.0] {
            let rects = pad_rects_scaled(size, scale);
            for (i, (_, [x, y, w, h])) in rects.iter().enumerate() {
                assert!(*x >= 0.0 && *y >= 0.0 && x + w <= 324.001 && y + h <= 720.001);
                assert_eq!(
                    pad_button_at_scaled((x + w / 2.0, y + h / 2.0), size, scale),
                    Some(PAD_BUTTONS[i].0)
                );
                for (_, [ox, oy, ow, oh]) in rects.iter().skip(i + 1) {
                    let overlap_w = (x + w).min(ox + ow) - x.max(*ox);
                    let overlap_h = (y + h).min(oy + oh) - y.max(*oy);
                    assert!(
                        overlap_w < 0.001 || overlap_h < 0.001,
                        "overlapping buttons at scale {scale}"
                    );
                }
            }
        }
        assert!(pad_rects_scaled(size, 2.0)[0].1[2] > pad_rects(size)[0].1[2]);
    }

    #[test]
    fn the_virtual_pad_presses_the_button_under_the_cursor() {
        let size = [1000, 1000];
        let mut pad = VirtualPad::default();
        assert_eq!(pad.press((500.0, 300.0), size), None, "the middle is the game's");
        assert_eq!(
            pad.press((880.0, 870.0), size),
            Some((GamepadButton::South, true)),
            "the first press announces the pad"
        );
        assert_eq!(pad.release(), Some(GamepadButton::South));
        assert_eq!(
            pad.press((120.0, 670.0), size),
            Some((GamepadButton::DPadUp, false))
        );
        assert!(pad.holding());
        // Every rect lies in the picture (to f32 rounding of the normalized table).
        for (name, [x, y, w, h]) in pad_rects(size) {
            assert!(
                x >= 0.0 && y >= 0.0 && x + w <= 1000.001 && y + h <= 1000.001,
                "{name}: {x} {y} {w} {h}"
            );
        }
    }
}
