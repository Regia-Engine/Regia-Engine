// SPDX-License-Identifier: MPL-2.0
//! The [`InputPlugin`]: resources, system sets and `PreUpdate` wiring for the input layer
//! (`docs/specs/11-platform-and-input.md` §§ 6–7).
//!
//! Pipeline (`docs/specs/11-platform-and-input.md` §§ 6–7):
//!
//! `PreUpdate`, in order:
//!
//! 1. [`InputSet::Backends`] — the platform gamepad backends poll their native sources (gilrs on
//!    desktop, android-activity + `InputManager` JNI on Android) and translate events into the
//!    platform's raw gamepad messages;
//! 2. [`InputSet::Devices`] — keyboard, mouse, touch (with gestures) and gamepad state are
//!    updated from the messages, and [`LastUsedDevice`] is tracked;
//! 3. [`InputSet::Rebinding`] — a pending [`InputRebinder`] capture is applied;
//! 4. [`InputSet::Evaluation`] — only on frames whose fixed loop will run no step (a fast
//!    display banking less than one step, a paused app): the action values are re-evaluated
//!    with a zero delta so edges refresh and one-frame injections apply, and the mouse delta is
//!    cleared.
//!
//! `FixedUpdate`, once per fixed step, in [`InputSet::Evaluation`]:
//!
//! 5. bindings are evaluated into [`Actions`] with that step's delta — triggers, holds, taps,
//!    pulses, smoothing and assists all advance in simulation time, and an edge (a press, say)
//!    is true for exactly one fixed step — then the mouse delta is cleared. A multi-step frame
//!    applies its mouse motion once, on its last step, so `Update` readers (which see the last
//!    step) see the motion too (ADR-5599).

use regia_app::prelude::*;
use regia_platform::{Vfs, VfsPath};

use crate::actions::{Actions, clear_mouse_delta, evaluate_actions, evaluate_actions_unless_stepped};
use crate::config::{GAMEPLAY_CONTEXT, InputConfig};
use crate::device::{Keyboard, LastUsedDevice, Mouse, update_keyboard, update_mouse};
use crate::gamepad::{Gamepads, update_gamepads};
use crate::rebind::{InputOverrides, InputRebinder, OVERRIDES_PATH, apply_rebinding};
use crate::touch::{TouchMouseEmulation, Touches, update_touches};

/// System sets of [`InputPlugin`]. `Backends`, `Devices` and `Rebinding` run inside `PreUpdate`,
/// configured to run in declaration order; `Evaluation` runs in `FixedUpdate`, once per fixed
/// step (see [`evaluate_actions`]).
///
/// Gameplay systems that must see this frame's action values order themselves
/// `after(InputSet::Evaluation)` — in `FixedUpdate` that is a hard requirement (evaluation
/// advances one step at a time there); in `PreUpdate` it orders after the zero-step-frame
/// fallback.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputSet {
    /// Platform gamepad backends poll and translate native events into messages.
    Backends,
    /// [`Keyboard`], [`Mouse`], [`Touches`] and [`Gamepads`] are updated from the messages.
    Devices,
    /// A pending rebind capture is applied to the config and persisted.
    Rebinding,
    /// Action values are evaluated from the device state in `FixedUpdate` (one fixed step at a
    /// time), or in `PreUpdate` when the frame runs no fixed step; then `Mouse::delta` is
    /// cleared.
    Evaluation,
}

/// Installs the input layer (`docs/specs/11-platform-and-input.md` §§ 6–7): the raw device
/// resources, the action-mapping config and the `PreUpdate` systems between them.
///
/// The consumed messages are registered here too (idempotent), so the plugin also works without
/// `regia_platform::PlatformPlugin` (tests, headless tools); the platform plugin still owns
/// producing the messages. When a [`Vfs`] is already mounted, saved rebinding overrides
/// (`user://input_overrides.ron`) are merged over `config` at build time.
///
/// ```
/// use regia_app::prelude::*;
/// use regia_input::prelude::*;
///
/// let config = InputConfig::new()
///     .action("Move", ActionKind::Axis2D)
///     .action("Jump", ActionKind::Button)
///     .binding(Binding::new("Move", InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD")))
///     .binding(Binding::new("Jump", InputSource::key("Space")));
///
/// let mut app = App::new();
/// app.add_plugins(InputPlugin::new(config));
/// // In gameplay systems: actions.pressed("Jump"), actions.axis2d("Move"), ...
/// ```
pub struct InputPlugin {
    config: InputConfig,
}

impl InputPlugin {
    /// Creates the plugin with the given action mapping; the `Gameplay` context starts enabled
    /// for player 0.
    pub fn new(config: InputConfig) -> Self {
        Self { config }
    }
}

impl Default for InputPlugin {
    /// An empty config (no actions); replace the `InputConfig` resource to extend it at runtime.
    fn default() -> Self {
        Self {
            config: InputConfig::default(),
        }
    }
}

impl Plugin for InputPlugin {
    fn build(&self, app: &mut App) {
        let mut config = self.config.clone();

        // Merge persisted rebinding overrides when a Vfs is already mounted (PlatformPlugin is
        // built first in DefaultPlugins; in tests without one, overrides stay memory-only).
        let mut load_failed = false;
        let loaded_overrides = app.world().get_resource::<Vfs>().and_then(|vfs| {
            let path = VfsPath::parse(OVERRIDES_PATH).ok()?;
            match InputOverrides::load_or_recover(vfs, &path) {
                Ok(overrides) => {
                    let applied = config.merge_overrides(&overrides);
                    if applied > 0 {
                        tracing::info!(
                            target: "regia_input",
                            "{applied} user input override(s) merged over the project config"
                        );
                    }
                    Some(overrides)
                }
                Err(err) => {
                    tracing::error!(
                        target: "regia_input",
                        "loading input overrides failed ({err}); using the project config and \
                         leaving the file untouched (rebinds will not be saved)"
                    );
                    load_failed = true;
                    Some(InputOverrides::default())
                }
            }
        });

        app.add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::MouseMotion>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::WindowFocused>()
            .add_message::<regia_platform::GamepadConnected>()
            .add_message::<regia_platform::GamepadDisconnected>()
            .add_message::<regia_platform::GamepadButtonChanged>()
            .add_message::<regia_platform::GamepadAxisChanged>()
            .add_message::<regia_platform::RumbleRequest>()
            .add_message::<crate::touch::GestureEvent>()
            .init_resource::<Keyboard>()
            .init_resource::<Mouse>()
            .init_resource::<Touches>()
            .init_resource::<Gamepads>()
            .init_resource::<Actions>()
            .init_resource::<crate::actions::MouseDeltaSchedule>()
            .init_resource::<crate::InputAssists>()
            .init_resource::<crate::InputInjection>()
            .init_resource::<crate::UiPointerGate>()
            .init_resource::<crate::actions::PlayerInputs>()
            .init_resource::<crate::actions::InputContexts>()
            // Written by regia_ui before evaluation; empty without it (ADR-7976).
            .init_resource::<crate::PointerClaims>()
            .insert_resource({
                let mut rebinder = InputRebinder::default();
                if load_failed {
                    rebinder.block_persist();
                }
                rebinder
            })
            .init_resource::<TouchMouseEmulation>()
            .init_resource::<LastUsedDevice>()
            .insert_resource(loaded_overrides.unwrap_or_default())
            .insert_resource(crate::rebind::InputDefaults(self.config.clone()))
            .insert_resource(config)
            .configure_sets(
                PreUpdate,
                (
                    InputSet::Backends,
                    InputSet::Devices,
                    InputSet::Rebinding,
                    InputSet::Evaluation,
                )
                    .chain(),
            )
            .add_systems(First, crate::actions::clear_frame_edges)
            // Chained: touch mouse emulation writes `Mouse` after the physical mouse update, and
            // every device system writes `LastUsedDevice`, so an unordered set made a mixed
            // frame's result depend on the executor (concurrency audit 11-03 M3, L5). The fixed
            // order also fixes the tie: the last system with activity this frame wins.
            .add_systems(
                PreUpdate,
                (update_keyboard, update_mouse, update_touches, update_gamepads)
                    .chain()
                    .in_set(InputSet::Devices),
            )
            .add_systems(
                PreUpdate,
                crate::touch_controls::apply_touch_controls
                    .after(update_touches)
                    .in_set(InputSet::Devices),
            )
            .add_systems(PreUpdate, apply_rebinding.in_set(InputSet::Rebinding))
            // Action evaluation advances the simulation's action state one *fixed step* at a
            // time (see `evaluate_actions`): the evaluation lives in `FixedUpdate`. On frames
            // whose fixed loop runs no step — a fast display banking less than one step, a
            // paused app — the chained fallback evaluates in `PreUpdate` with a zero delta, and
            // the mouse delta is cleared there too (on stepped frames the `FixedUpdate` chain
            // clears it after the frame's last step read it; clearing it here as well would zero
            // it before that read). `plan_mouse_delta` predicts the frame's step count first, so
            // the motion lands on the last step and `Update` readers see it (ADR-5599).
            .add_systems(
                PreUpdate,
                (
                    crate::actions::plan_mouse_delta,
                    (evaluate_actions_unless_stepped, clear_mouse_delta)
                        .chain()
                        .run_if(no_fixed_step_this_frame),
                )
                    .chain()
                    .in_set(InputSet::Evaluation),
            )
            .add_systems(
                FixedUpdate,
                (
                    crate::actions::stage_mouse_delta,
                    evaluate_actions,
                    clear_mouse_delta,
                )
                    .chain()
                    .in_set(InputSet::Evaluation),
            );

        // The default context starts enabled for player 0.
        if !self.config.contexts.is_empty() {
            let mut contexts = crate::actions::InputContexts::default();
            if self.config.contexts.contains_key(GAMEPLAY_CONTEXT) {
                contexts.push(0, GAMEPLAY_CONTEXT);
            }
            app.insert_resource(contexts);
        }

        // No backend in a `devices`-less desktop build (the server): no gamepads.
        #[cfg(any(feature = "devices", target_os = "android"))]
        app.add_systems(PreUpdate, backend_poll().in_set(InputSet::Backends));

        // Desktop: the SpaceMouse (spec 29); a no-op until a device appears. Added only when
        // the app did not add it first (with its own backend).
        #[cfg(all(feature = "devices", not(target_os = "android")))]
        if !app.has_plugin(std::any::type_name::<crate::spacemouse::SpaceMousePlugin>()) {
            app.add_plugins(crate::spacemouse::SpaceMousePlugin::default());
        }

        // Desktop: create the gilrs context up front (a machine without udev — containers,
        // headless CI — simply reports no gamepads).
        #[cfg(all(feature = "devices", not(target_os = "android")))]
        match gilrs::Gilrs::new() {
            Ok(gilrs) => {
                crate::gamepad::install_context(
                    app.world_mut(),
                    crate::gamepad::GilrsContext::new(Some(gilrs)),
                );
            }
            Err(err) => {
                tracing::info!(target: "regia_input", "gamepads unavailable (gilrs: {err})");
                crate::gamepad::install_context(app.world_mut(), crate::gamepad::GilrsContext::default());
            }
        }
    }
}

/// The platform gamepad backend poll of this build: gilrs on desktop, the android-activity
/// translation on Android. (No backend → no gamepads, e.g. in headless tests.)
#[cfg(all(feature = "devices", not(target_os = "android")))]
fn backend_poll() -> crate::gamepad::GilrsPoll {
    crate::gamepad::poll_gilrs
}

/// Run condition: the fixed loop runs no step this frame (a fast display banking less than one
/// step, a paused app with an empty bank, or no sim step configured). The `PreUpdate`
/// evaluation fallback and the mouse-delta clear run instead of the `FixedUpdate` chain.
fn no_fixed_step_this_frame(time: Res<Time>, fixed: Option<Res<FixedTime>>) -> bool {
    !fixed.is_some_and(|fixed| fixed.would_run(time.sim_delta()))
}

#[cfg(target_os = "android")]
fn backend_poll() -> crate::gamepad::AndroidPoll {
    crate::gamepad::poll_android_gamepads
}
