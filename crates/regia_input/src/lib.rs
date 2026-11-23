// SPDX-License-Identifier: MPL-2.0
//! Raw input (keyboard, mouse, touch, gamepads) and the action-mapping system.
//!
//! Specification: `docs/specs/11-platform-and-input.md` §§ 6–7.
//!
//! * Device state ([`Keyboard`], [`Mouse`], [`Touches`], [`Gamepads`]) is built from the
//!   platform's input messages in `PreUpdate`; the gamepad backends (gilrs on desktop,
//!   android-activity + `InputManager` JNI on Android) produce the raw gamepad messages.
//! * The action layer ([`InputConfig`], contexts, modifiers, triggers) turns device state into
//!   per-player [`Actions`]; [`InputInjection`] drives actions programmatically (agent protocol,
//!   virtual controls) and [`InputRebinder`] persists user rebinding to `user://input_overrides.ron`.
//!
//! Axis convention throughout this crate: **x = right, y = forward/up** (gamepad stick Y is
//! up-positive; touch/gesture positions are window pixels with y down, as reported).
//!
//! # How the pieces fit
//!
//! The platform layer produces raw device messages, which [`update_keyboard`] / [`update_mouse`] /
//! [`update_gamepads`] / [`update_touches`] fold into the [`Devices`] resource in `PreUpdate`. The
//! action layer ([`InputConfig`] with contexts, bindings and modifiers) is then evaluated by
//! [`evaluate_actions`] into per-player [`Actions`] — the single thing gameplay systems read —
//! once per fixed step in `FixedUpdate` (or once per frame with a zero delta in `PreUpdate` when
//! the frame runs no fixed step). [`InputInjection`] feeds synthetic input through the same
//! pipeline (agent protocol, virtual controls), and [`InputRebinder`] captures the next raw
//! input, writes it to [`InputOverrides`] (persisted to `user://input_overrides.ron`) and
//! re-merges the live config. Because the axis convention is fixed at the boundary, a game reads
//! one value and never flips Y itself.
//!
//! See `book/user-guide/input.md` (input messages, actions, touch). SpaceMouse navigation
//! (desktop) is in `regia_input::spacemouse` (`docs/specs/29-spacemouse.md`, `book/user-guide/spacemouse.md`).
//!
//! [`InputPlugin`] wires device sampling into `PreUpdate` and action evaluation into
//! `FixedUpdate` (one fixed step at a time; `Update` readers see the last step of the frame);
//! gameplay systems read actions in `Update` or `FixedUpdate`:
//!
//! ```
//! use regia_app::prelude::*;
//! use regia_input::prelude::*;
//!
//! let mut app = App::new();
//! app.add_plugins(InputPlugin::new(
//!     InputConfig::new()
//!         .action("Jump", ActionKind::Button)
//!         .binding(Binding::new("Jump", InputSource::key("Space"))),
//! ));
//! app.update();
//! assert!(!app.world().resource::<Actions>().pressed("Jump")); // nothing pressed yet
//! ```

mod actions;
#[cfg(any(target_os = "android", test))]
mod android_map;
mod assist;
mod config;
mod deterministic;
mod device;
mod gamepad;
mod injection;
mod plugin;
// Mouse buttons the runtime UI claimed from press to release (ADR-7976).
mod pointer_claims;
mod rebind;
#[cfg(all(feature = "devices", not(target_os = "android")))]
pub mod spacemouse;
mod touch;
mod touch_controls;
mod ui_routing;

pub use actions::{
    ActionPhase, ActionState, Actions, DeviceAssignment, InputContexts, PlayerActions, PlayerInputs,
    clear_frame_edges, clear_mouse_delta, evaluate_actions, evaluate_actions_unless_stepped,
};
pub use assist::{ActionAssist, InputAssists, PressMode, RepeatRate};
pub use config::{
    ActionKind, Binding, BindingKey, DeadZone, DeadZoneMode, DeviceClass, GAMEPLAY_CONTEXT, InputConfig,
    InputContext, InputSource, LookSettings, Modifier, ModifierFrame, ResponseCurve, SpaceMouseConfig, Stick,
    Swizzle, Trigger, apply_modifiers,
};
pub use config::{SKIP_VIDEO_ACTION, VIDEO_CONTEXT};
pub use device::{
    Devices, InputDevice, KeyCode, Keyboard, LastUsedDevice, Mouse, PIXELS_PER_WHEEL_LINE, update_keyboard,
    update_mouse,
};
pub use gamepad::{
    BUTTON_THRESHOLD, Gamepads, MAX_QUEUED_RUMBLE, Pad, PadButton, PhoneVibration, update_gamepads,
};
pub use injection::{InputInjection, ScheduledInjection};
pub use plugin::{InputPlugin, InputSet};
pub use pointer_claims::{PointerClaims, mouse_button_name};
pub use rebind::{
    AxisTuning, BUTTON_AXIS_DEAD_ZONE, DEFAULT_DEAD_ZONE, MAX_DEAD_ZONE, MOUSE_CAPTURE_TRAVEL,
    OVERRIDES_VERSION, PROFILES_DIR, RebindSlot, SlotOverrides, axis_tuning, binding_conflicts,
    binding_label_in, chord_helper_in, has_dead_zone_axis, list_profiles, load_profile, profile_path,
    save_profile, set_axis_tuning, start_rebind_slot,
};
pub use rebind::{
    InputDefaults, InputOverrides, InputRebinder, OVERRIDES_PATH, RebindError, RebindMode, RebindOutcome,
    RebindProgress, apply_rebinding, binding_label, cancel_rebind, chord_helper, is_chord_helper,
    reset_action, reset_all, source_label, start_rebind,
};
// The `input_overrides.ron` shape for `cargo xtask check-formats` (ADR-2316).
pub use rebind::{input_overrides_migration_sources, input_overrides_schema_fingerprint};
// Damaged `input_overrides.ron` copies kept at start-up (ADR-3102).
pub use rebind::MAX_BAD_OVERRIDE_COPIES;
pub use touch::{
    DOUBLE_TAP_GAP_SECS, DOUBLE_TAP_SLOP_PX, GestureEvent, LONG_PRESS_SECS, LONG_PRESS_SLOP_PX,
    PAN_MIN_DELTA_PX, PINCH_MIN_CHANGE, SWIPE_MAX_SECS, SWIPE_MIN_PX, TAP_MAX_SECS, TAP_SLOP_PX,
    TouchMouseEmulation, TouchPoint, Touches, update_touches,
};
pub use touch_controls::{
    GestureBinding, GestureKind, TouchControlKind, TouchControlOutput, TouchControls, TouchRect, TouchZone,
};

/// `#[derive(InputAction)]` (`regia_macros`): implements [`InputAction`] for a fieldless enum.
pub use regia_macros::InputAction;
pub use ui_routing::{
    InputAction, MENU_BUTTON_ACTIONS, MENU_CANCEL, MENU_CONTEXT, MENU_DOWN, MENU_LEFT, MENU_NAVIGATE,
    MENU_NEXT, MENU_PREV, MENU_RIGHT, MENU_SUBMIT, MENU_UP, UiPointerGate,
};

#[cfg(target_os = "android")]
pub use android_map::{android_keycode, button_from_akeycode, joystick_from_touch};
#[cfg(target_os = "android")]
pub use gamepad::poll_android_gamepads;

/// Common imports for code that uses the input layer.
pub mod prelude {
    pub use crate::{
        actions::{ActionPhase, Actions, DeviceAssignment, InputContexts, PlayerInputs},
        assist::{ActionAssist, InputAssists, PressMode, RepeatRate},
        config::{
            ActionKind, Binding, DeadZone, InputConfig, InputContext, InputSource, LookSettings, Modifier,
            Stick, Trigger,
        },
        device::{KeyCode, Keyboard, LastUsedDevice, Mouse},
        gamepad::{Gamepads, Pad, PhoneVibration},
        injection::InputInjection,
        plugin::{InputPlugin, InputSet},
        rebind::{InputOverrides, InputRebinder, RebindOutcome},
        touch::{GestureEvent, Touches},
        ui_routing::{InputAction, MENU_CONTEXT, MENU_UP, UiPointerGate},
    };
    /// `#[derive(InputAction)]`.
    pub use regia_macros::InputAction;
    /// Mouse buttons, re-exported for building bindings.
    pub use regia_platform::MouseButton;
    /// Gamepad buttons and axes, re-exported from the platform layer for bindings.
    pub use regia_platform::{GamepadAxis, GamepadButton};
}

mod keyboard_layout;
pub use keyboard_layout::KeyboardLayout;
