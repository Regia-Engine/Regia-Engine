// SPDX-License-Identifier: MPL-2.0
//! Gamepad state (`docs/specs/11-platform-and-input.md` § 6): the [`Gamepads`] resource, the
//! `PreUpdate` system that fills it from the platform's raw gamepad messages, and the platform
//! backends that produce those messages — gilrs on desktop, android-activity events plus
//! `InputManager` JNI on Android.
//!
//! Backends are deliberately message-driven: `poll_gilrs` / `poll_android_gamepads` translate
//! native events into `regia_platform` messages, and [`update_gamepads`] drains them. Tests
//! (and the agent protocol) can drive gamepads by writing the same messages, no hardware needed.
//!
//! Axis convention (see `regia_platform::GamepadAxis`): x = right, **y = up**; the Android
//! backend flips the OS's down-positive Y.

use std::collections::BTreeMap;
#[cfg(any(target_os = "android", test))]
use std::collections::BTreeSet;
use std::time::Duration;

use regia_app::prelude::*;
#[cfg(any(feature = "devices", target_os = "android", test))]
use regia_platform::ButtonState;
use regia_platform::{
    GamepadAxis, GamepadAxisChanged, GamepadButton, GamepadButtonChanged, GamepadConnected,
    GamepadDisconnected, RumbleRequest,
};

use crate::device::LastUsedDevice;

/// The value above which an analog button counts as "pressed" ([`Gamepads::pressed`]).
pub const BUTTON_THRESHOLD: f32 = 0.5;

/// Most rumble requests [`Gamepads`] keeps queued for the backend (the oldest are dropped first).
/// A backend drains the queue every frame; without one (the `devices`-less server build) it
/// would otherwise grow for the life of the process.
pub const MAX_QUEUED_RUMBLE: usize = 64;

/// State of one gamepad button: analog value plus this-frame edges.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PadButton {
    /// Analog magnitude, 0..1 (triggers are analog; digital buttons are 0 or 1).
    pub value: f32,
    /// The button crossed [`BUTTON_THRESHOLD`] downwards→upwards this frame.
    pub just_pressed: bool,
    /// The button crossed back below the threshold this frame.
    pub just_released: bool,
}

/// State of one connected gamepad.
#[derive(Debug, Clone, Default)]
pub struct Pad {
    /// Human-readable name as reported by the backend.
    pub name: String,
    /// Button state by standard-layout button.
    pub buttons: BTreeMap<GamepadButton, PadButton>,
    /// Axis values by standard-layout axis.
    pub axes: BTreeMap<GamepadAxis, f32>,
}

impl Pad {
    /// The analog value of `button` (0 when the pad never reported it).
    pub fn button_value(&self, button: GamepadButton) -> f32 {
        self.buttons.get(&button).map_or(0.0, |b| b.value)
    }

    /// True while `button` is past [`BUTTON_THRESHOLD`].
    pub fn pressed(&self, button: GamepadButton) -> bool {
        self.button_value(button) > BUTTON_THRESHOLD
    }

    /// True on the frame `button` crossed the threshold downwards→upwards.
    pub fn just_pressed(&self, button: GamepadButton) -> bool {
        self.buttons.get(&button).is_some_and(|b| b.just_pressed)
    }

    /// True on the frame `button` crossed back below the threshold.
    pub fn just_released(&self, button: GamepadButton) -> bool {
        self.buttons.get(&button).is_some_and(|b| b.just_released)
    }

    /// The value of `axis` (0 when never reported).
    pub fn axis(&self, axis: GamepadAxis) -> f32 {
        self.axes.get(&axis).copied().unwrap_or(0.0)
    }

    /// True while the pad is connected.
    pub fn is_connected(&self) -> bool {
        !self.name.is_empty()
    }
}

/// Every connected gamepad and its state, updated in `PreUpdate` (specs 11 § 6).
#[derive(Resource, Debug, Default)]
pub struct Gamepads {
    pub(crate) pads: BTreeMap<u64, Pad>,
    /// Rumble requests queued this frame; the active backend drains them after the device update.
    pub(crate) rumble: Vec<(u64, f32, Duration)>,
    /// The player's haptics intensity (ADR-2646): every queued strength is multiplied by it.
    /// `None` is full strength.
    rumble_scale: Option<f32>,
}

impl Gamepads {
    /// The state of one pad, if connected.
    pub fn pad(&self, device: u64) -> Option<&Pad> {
        self.pads.get(&device).filter(|p| p.is_connected())
    }

    /// All connected pads, by device id.
    pub fn pads(&self) -> impl Iterator<Item = (u64, &Pad)> {
        self.pads
            .iter()
            .filter(|(_, p)| p.is_connected())
            .map(|(id, p)| (*id, p))
    }

    /// How many pads are connected.
    pub fn connected(&self) -> usize {
        self.pads().count()
    }

    /// The name of a connected pad.
    pub fn name(&self, device: u64) -> Option<&str> {
        self.pad(device).map(|p| p.name.as_str())
    }

    /// The first connected pad id (gamepads are usually played one at a time).
    pub fn first(&self) -> Option<u64> {
        self.pads().map(|(id, _)| id).next()
    }

    /// Whether `button` is pressed on `device`.
    pub fn pressed(&self, device: u64, button: GamepadButton) -> bool {
        self.pad(device).is_some_and(|p| p.pressed(button))
    }

    /// The first pad on which `button` crossed the threshold this frame.
    pub fn just_pressed(&self, button: GamepadButton) -> Option<u64> {
        self.pads()
            .find(|(_, p)| p.just_pressed(button))
            .map(|(id, _)| id)
    }

    /// The first pad on which `button` was released this frame.
    pub fn just_released(&self, button: GamepadButton) -> Option<u64> {
        self.pads()
            .find(|(_, p)| p.just_released(button))
            .map(|(id, _)| id)
    }

    /// The value of `axis` on `device` (0 if unknown).
    pub fn axis(&self, device: u64, axis: GamepadAxis) -> f32 {
        self.pad(device).map_or(0.0, |p| p.axis(axis))
    }

    /// Marks `device` connected under `name`.
    ///
    /// Device backends do this from [`GamepadConnected`]. A headless host with no backend, and
    /// tests, use this so [`Gamepads::pads`] yields the pad. An empty `name` leaves the pad
    /// disconnected ([`Pad::is_connected`]).
    pub fn connect_pad(&mut self, device: u64, name: impl Into<String>) {
        let pad = self.pads.entry(device).or_default();
        pad.name = name.into();
    }

    /// Rumble requests queued for the backend and not yet applied, oldest first.
    ///
    /// Each item is `(device id, strength 0..=1 after the haptics scale, duration)`.
    /// [`Gamepads::rumble`] appends here; a device backend drains the queue.
    #[must_use]
    pub fn queued_rumbles(&self) -> &[(u64, f32, Duration)] {
        &self.rumble
    }

    /// Queues a rumble on `device` with `strength` (0..1) for `duration`; the active backend
    /// applies it (fire and forget — rumble errors are logged, not returned). At most
    /// [`MAX_QUEUED_RUMBLE`] requests wait; the oldest are dropped first.
    pub fn rumble(&mut self, device: u64, strength: f32, duration: Duration) {
        let scale = self.rumble_scale();
        self.rumble
            .push((device, strength.clamp(0.0, 1.0) * scale, duration));
        self.trim_rumble();
    }

    /// Sets the player's haptics intensity (ADR-2646, an accessibility choice): `0` turns rumble
    /// off, `1` (the default) plays it as requested. Every later request, from [`Gamepads::rumble`]
    /// or a `RumbleRequest` message, is scaled by it. Clamped into `0..=1`; NaN reads as `1`.
    pub fn set_rumble_scale(&mut self, scale: f32) {
        let scale = if scale.is_nan() {
            1.0
        } else {
            scale.clamp(0.0, 1.0)
        };
        self.rumble_scale = (scale < 1.0).then_some(scale);
    }

    /// The haptics intensity in effect (`0..=1`, see [`Gamepads::set_rumble_scale`]).
    pub fn rumble_scale(&self) -> f32 {
        self.rumble_scale.unwrap_or(1.0)
    }

    /// Drops the oldest queued rumble requests beyond [`MAX_QUEUED_RUMBLE`]: a build without a
    /// device backend (the server) never drains the queue.
    fn trim_rumble(&mut self) {
        let excess = self.rumble.len().saturating_sub(MAX_QUEUED_RUMBLE);
        if excess > 0 {
            self.rumble.drain(..excess);
        }
    }

    /// Test helper: pretend one axis of one pad has the given value.
    #[cfg(test)]
    pub(crate) fn set_axis_for_test(&mut self, device: u64, axis: GamepadAxis, value: f32) {
        let pad = self.pads.entry(device).or_default();
        pad.name = "test pad".into();
        pad.axes.insert(axis, value);
    }
}

/// `PreUpdate` (set [`crate::InputSet::Devices`]): drains the platform's raw gamepad messages
/// (including [`RumbleRequest`]s, which are queued for the backend) into the [`Gamepads`]
/// resource and tracks [`LastUsedDevice`].
#[tracing::instrument(target = "regia_input", level = "trace", skip_all)]
pub fn update_gamepads(
    mut gamepads: ResMut<Gamepads>,
    mut connected: MessageReader<GamepadConnected>,
    mut disconnected: MessageReader<GamepadDisconnected>,
    mut buttons: MessageReader<GamepadButtonChanged>,
    mut axes: MessageReader<GamepadAxisChanged>,
    mut rumble: MessageReader<RumbleRequest>,
    mut last_used: ResMut<LastUsedDevice>,
) {
    // Clear the edges of every pad; they are recomputed from this frame's messages below.
    for pad in gamepads.pads.values_mut() {
        for button in pad.buttons.values_mut() {
            button.just_pressed = false;
            button.just_released = false;
        }
    }
    for event in connected.read() {
        tracing::info!(target: "regia_input", "gamepad {} connected: {}", event.device, event.name);
        let pad = gamepads.pads.entry(event.device).or_default();
        pad.name = event.name.clone();
    }
    // The four message kinds carry no order between them, so a button or axis change in the
    // same batch as its pad's disconnect (unplug while playing) would recreate the pad with the
    // value stuck. Ids disconnected this frame take no button or axis update; a pad that really
    // came back sends its events again on the next frame.
    let mut gone: Vec<u64> = Vec::new();
    for event in disconnected.read() {
        tracing::info!(target: "regia_input", "gamepad {} disconnected", event.device);
        // Drop the entry: ids are only stable while connected (Android), so keeping a cleared
        // pad per id would grow the map and the per-frame edge-clearing walk.
        gamepads.pads.remove(&event.device);
        gone.push(event.device);
    }
    for event in buttons.read() {
        if gone.contains(&event.device) {
            continue;
        }
        last_used.gamepad(event.device);
        let pad = gamepads.pads.entry(event.device).or_default();
        if pad.name.is_empty() {
            // The backend knows the pad (it is sending events) but its Connected message has not
            // been processed yet (or was never sent); keep it usable with a placeholder name.
            pad.name = "gamepad".into();
        }
        let entry = pad.buttons.entry(event.button).or_default();
        let sample = if event.value.is_finite() { event.value } else { 0.0 };
        let was_past = entry.value > BUTTON_THRESHOLD;
        let now_past = sample > BUTTON_THRESHOLD;
        entry.value = sample.clamp(0.0, 1.0);
        entry.just_pressed |= now_past && !was_past;
        entry.just_released |= !now_past && was_past;
    }
    for event in axes.read() {
        if gone.contains(&event.device) {
            continue;
        }
        last_used.gamepad(event.device);
        let pad = gamepads.pads.entry(event.device).or_default();
        if pad.name.is_empty() {
            pad.name = "gamepad".into();
        }
        let sample = if event.value.is_finite() { event.value } else { 0.0 };
        pad.axes.insert(event.axis, sample.clamp(-1.0, 1.0));
    }
    let scale = gamepads.rumble_scale();
    for request in rumble.read() {
        gamepads
            .rumble
            .push((request.device, request.strength * scale, request.duration));
    }
    gamepads.trim_rumble();
}

/// Applies queued rumble requests through the desktop gilrs backend (fire and forget).
#[cfg(all(feature = "devices", not(target_os = "android")))]
pub(crate) fn apply_rumble_gilrs(gilrs: &mut gilrs::Gilrs, requests: Vec<(u64, f32, Duration)>) {
    for (device, strength, duration) in requests {
        let ids = gilrs
            .gamepads()
            .filter(|(id, _)| usize::from(*id) == device as usize)
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        if ids.is_empty() {
            tracing::warn!(target: "regia_input", "rumble: gamepad {device} not connected");
            continue;
        }
        if !ids.iter().all(|id| gilrs.gamepad(*id).is_ff_supported()) {
            continue;
        }
        let magnitude = (strength.clamp(0.0, 1.0) * u16::MAX as f32).round() as u16;
        let ticks = gilrs::ff::Ticks::from_ms(duration.as_millis().min(u32::MAX as u128) as u32);
        let effect = gilrs::ff::EffectBuilder::new()
            .add_effect(gilrs::ff::BaseEffect {
                kind: gilrs::ff::BaseEffectType::Strong { magnitude },
                scheduling: gilrs::ff::Replay {
                    play_for: ticks,
                    ..Default::default()
                },
                envelope: Default::default(),
            })
            .gamepads(&ids)
            .finish(gilrs);
        match effect {
            Ok(effect) => {
                if let Err(err) = effect.play() {
                    tracing::warn!(target: "regia_input", "rumble play failed: {err}");
                }
            }
            Err(err) => tracing::warn!(target: "regia_input", "rumble setup failed: {err}"),
        }
    }
}

/// Applies queued rumble requests through the Android `InputDevice` vibrator.
#[cfg(target_os = "android")]
pub(crate) fn apply_rumble_android(gamepads: &mut Gamepads) {
    for (device, strength, duration) in std::mem::take(&mut gamepads.rumble) {
        if let Err(err) = regia_platform::gamepad_rumble(device as i32, strength, duration) {
            tracing::debug!(target: "regia_input", "gamepad {device} rumble: {err}");
        }
    }
}

// ---------------------------------------------------------------------------
// Desktop backend: gilrs
// ---------------------------------------------------------------------------

#[cfg(all(feature = "devices", not(target_os = "android")))]
mod gilrs_backend {
    use super::*;
    use gilrs::{Axis, Button, EventType, Gilrs};
    use regia_platform::{GamepadAxisChanged, GamepadButtonChanged, GamepadConnected, GamepadDisconnected};

    /// The gilrs context (not `Sync`, so it lives in the `World` as a non-send resource, inserted
    /// with [`install_context`] and read with `NonSendMut` so only the main thread touches it; a
    /// plain `Resource` would never be found by `NonSendMut`). `None` when gilrs could not
    /// initialise (no udev on this machine, headless test...): the gamepad layer then simply
    /// reports no pads. The `gilrs` handle is deliberately not part of this crate's public API
    /// (`docs/03` § 3: dependencies do not leak).
    #[derive(Default)]
    pub(crate) struct GilrsContext {
        gilrs: Option<Gilrs>,
        /// The latched trigger-axis range of every `(pad, axis)` the backend has seen
        /// ([`TriggerRange`]): backend state, owned here because only the poll loop updates it.
        trigger_ranges: std::collections::BTreeMap<(u64, GamepadAxis), TriggerRange>,
    }

    impl GilrsContext {
        /// A context wrapping `gilrs` (none when gilrs could not initialise).
        pub(crate) fn new(gilrs: Option<Gilrs>) -> Self {
            Self {
                gilrs,
                trigger_ranges: std::collections::BTreeMap::new(),
            }
        }
    }

    /// The trigger-axis range of one pad axis. A single sample's sign cannot identify whether a
    /// pad reports 0..1 or −1..1, so the range is *latched*: the first negative sample proves a
    /// −1..1 pad and every later sample — including non-negative ones — is shifted into 0..1.
    /// Until then the pad reads as 0..1 so its rest position stays 0 (audit finding 4, ADR-4768
    /// kept that property; fixrev review finding 3 made the mapping monotonic: shifting only
    /// negative samples collapsed a −1..1 pull to 0 at the midpoint).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub(crate) enum TriggerRange {
        /// No negative sample yet: the pad reads as 0..1, values pass through.
        #[default]
        Unit,
        /// A negative sample arrived: the pad is −1..1, every sample is shifted.
        Signed,
    }

    /// Maps one trigger-axis sample through the pad's latched range, updating the latch: a
    /// negative sample latches [`TriggerRange::Signed`]. Pure and deterministic (multiplies only).
    pub(crate) fn normalize_trigger(latch: &mut TriggerRange, value: f32) -> f32 {
        // A non-finite sample is not a pull, and must not latch the range (audfix-input).
        if !value.is_finite() {
            return 0.0;
        }
        if value < 0.0 {
            *latch = TriggerRange::Signed;
        }
        match latch {
            TriggerRange::Unit => value.clamp(0.0, 1.0),
            TriggerRange::Signed => (value * 0.5 + 0.5).clamp(0.0, 1.0),
        }
    }

    /// Stores `context` where [`poll_gilrs`]'s `NonSendMut` parameter can find it.
    pub(crate) fn install_context(world: &mut World, context: GilrsContext) {
        world.insert_non_send(context);
    }

    /// Maps a gilrs button to the standard layout. Digital shoulders are `LeftTrigger2` /
    /// `RightTrigger2` in gilrs' naming (the analog triggers are `LeftTrigger`/`RightTrigger`).
    fn map_button(button: Button) -> Option<GamepadButton> {
        Some(match button {
            Button::South => GamepadButton::South,
            Button::East => GamepadButton::East,
            Button::West => GamepadButton::West,
            Button::North => GamepadButton::North,
            Button::LeftTrigger2 => GamepadButton::LeftShoulder,
            Button::RightTrigger2 => GamepadButton::RightShoulder,
            Button::LeftTrigger => GamepadButton::LeftTrigger,
            Button::RightTrigger => GamepadButton::RightTrigger,
            Button::LeftThumb => GamepadButton::LeftStick,
            Button::RightThumb => GamepadButton::RightStick,
            Button::DPadUp => GamepadButton::DPadUp,
            Button::DPadDown => GamepadButton::DPadDown,
            Button::DPadLeft => GamepadButton::DPadLeft,
            Button::DPadRight => GamepadButton::DPadRight,
            Button::Start => GamepadButton::Start,
            Button::Select => GamepadButton::Select,
            Button::Mode => GamepadButton::Mode,
            _ => return None,
        })
    }

    /// Maps a gilrs axis to the standard layout (Y is up-positive through gilrs' gamepad DB).
    fn map_axis(axis: Axis) -> Option<GamepadAxis> {
        Some(match axis {
            Axis::LeftStickX => GamepadAxis::LeftStickX,
            Axis::LeftStickY => GamepadAxis::LeftStickY,
            Axis::RightStickX => GamepadAxis::RightStickX,
            Axis::RightStickY => GamepadAxis::RightStickY,
            Axis::LeftZ => GamepadAxis::LeftTrigger,
            Axis::RightZ => GamepadAxis::RightTrigger,
            Axis::DPadX => GamepadAxis::DPadX,
            Axis::DPadY => GamepadAxis::DPadY,
            Axis::Unknown => return None,
        })
    }

    /// Maps a trigger-axis (LeftZ/RightZ) sample to the standard 0..1 range through the pad's
    /// latched range (see [`TriggerRange`]).
    fn normalize_trigger_for(
        trigger_ranges: &mut std::collections::BTreeMap<(u64, GamepadAxis), TriggerRange>,
        device: u64,
        axis: GamepadAxis,
        value: f32,
    ) -> f32 {
        let latch = trigger_ranges.entry((device, axis)).or_default();
        normalize_trigger(latch, value)
    }

    fn button_state(value: f32) -> ButtonState {
        if value > super::BUTTON_THRESHOLD {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        }
    }

    /// `PreUpdate` (set [`crate::InputSet::Backends`]): polls gilrs and translates its events
    /// into the platform's gamepad messages. On the first poll, every already-connected pad is
    /// announced. Rumble requests queued on [`Gamepads`] are applied here too.
    #[tracing::instrument(target = "regia_input", level = "trace", skip_all)]
    pub(crate) fn poll_gilrs(
        mut context: Option<NonSendMut<GilrsContext>>,
        mut gamepads: ResMut<Gamepads>,
        mut connected: MessageWriter<GamepadConnected>,
        mut disconnected: MessageWriter<GamepadDisconnected>,
        mut buttons: MessageWriter<GamepadButtonChanged>,
        mut axes: MessageWriter<GamepadAxisChanged>,
    ) {
        let Some(context) = context.as_mut() else {
            gamepads.rumble.clear();
            return;
        };
        // Disjoint field borrows: the event loop holds the gilrs handle while the trigger
        // latches are updated per sample.
        let GilrsContext {
            gilrs,
            trigger_ranges,
        } = &mut **context;
        let Some(gilrs) = gilrs.as_mut() else {
            gamepads.rumble.clear();
            return;
        };
        // Announce pads that were connected before the first poll (`update_gamepads` fills the
        // resource from the messages, which makes this a once-only block).
        if gamepads.pads.is_empty() {
            for (id, pad) in gilrs.gamepads() {
                connected.write(GamepadConnected {
                    device: usize::from(id) as u64,
                    name: pad.name().to_string(),
                });
            }
        }
        while let Some(gilrs::Event { id, event, .. }) = gilrs.next_event() {
            let device = usize::from(id) as u64;
            match event {
                EventType::Connected => {
                    let name = gilrs.gamepad(id).name().to_string();
                    tracing::info!(target: "regia_input", "gamepad {device} connected: {name}");
                    connected.write(GamepadConnected { device, name });
                }
                EventType::Disconnected => {
                    tracing::info!(target: "regia_input", "gamepad {device} disconnected");
                    disconnected.write(GamepadDisconnected { device });
                }
                EventType::ButtonPressed(button, _) => {
                    if let Some(mapped) = map_button(button) {
                        buttons.write(GamepadButtonChanged {
                            device,
                            button: mapped,
                            state: ButtonState::Pressed,
                            value: 1.0,
                        });
                    }
                }
                EventType::ButtonReleased(button, _) => {
                    if let Some(mapped) = map_button(button) {
                        buttons.write(GamepadButtonChanged {
                            device,
                            button: mapped,
                            state: ButtonState::Released,
                            value: 0.0,
                        });
                    }
                }
                EventType::ButtonChanged(button, value, _) => {
                    if let Some(mapped) = map_button(button) {
                        buttons.write(GamepadButtonChanged {
                            device,
                            button: mapped,
                            state: button_state(value),
                            value,
                        });
                    }
                }
                EventType::AxisChanged(axis, value, _) => {
                    if let Some(mapped) = map_axis(axis) {
                        // gilrs reports stick Y with up = +1 through its gamepad DB; triggers
                        // (LeftZ/RightZ) arrive as −1..1 on some pads and 0..1 on others, with
                        // the range latched per pad and axis once a negative sample proves it.
                        let value = match mapped {
                            GamepadAxis::LeftTrigger | GamepadAxis::RightTrigger => {
                                normalize_trigger_for(trigger_ranges, device, mapped, value)
                            }
                            _ => value,
                        };
                        axes.write(GamepadAxisChanged {
                            device,
                            axis: mapped,
                            value,
                        });
                    }
                }
                _ => {}
            }
        }
        // Rumble last (needs the exclusive gilrs borrow).
        if !gamepads.rumble.is_empty() {
            let requests = std::mem::take(&mut gamepads.rumble);
            apply_rumble_gilrs(gilrs, requests);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Fixrev review finding 3: the mapping must be monotonic per pad. A 0..1 pad never
        /// latches and its rest position (0.0) stays 0.0, not a half-held trigger (audit
        /// finding 4, ADR-4768, kept). A −1..1 pad latches on its first negative sample — the
        /// rest position is −1 — and from then on the *whole* range is shifted, so the pull no
        /// longer collapses to 0.0 at the midpoint (the old sign-of-one-sample guess mapped
        /// −0.1 → 0.45 and the next sample 0.0 → 0.0).
        #[test]
        fn trigger_axis_normalization_is_monotonic_per_pad() {
            // A 0..1 pad: no negative sample ever arrives, values pass through.
            let mut unit = TriggerRange::default();
            assert_eq!(normalize_trigger(&mut unit, 0.0), 0.0, "rest on a 0..1 pad");
            assert_eq!(
                normalize_trigger(&mut unit, 0.2),
                0.2,
                "lower half is not rescaled"
            );
            assert_eq!(normalize_trigger(&mut unit, 1.0), 1.0, "full pull on a 0..1 pad");
            assert_eq!(unit, TriggerRange::Unit, "no negative sample: stays a 0..1 pad");

            // A −1..1 pad: rest (−1) latches immediately; the full sweep is monotonic.
            let mut signed = TriggerRange::default();
            let sweep = [-1.0, -0.5, -0.1, 0.0, 0.2, 0.5, 1.0];
            let mapped: Vec<f32> = sweep.iter().map(|&v| normalize_trigger(&mut signed, v)).collect();
            assert_eq!(mapped, vec![0.0, 0.25, 0.45, 0.5, 0.6, 0.75, 1.0]);
            assert!(mapped.windows(2).all(|w| w[0] <= w[1]), "monotonic: {mapped:?}");
            assert_eq!(
                normalize_trigger(&mut signed, 0.0),
                0.5,
                "the positive half keeps the shift after latching: no collapse at rest"
            );
            assert_eq!(normalize_trigger(&mut signed, 2.0), 1.0, "clamped");
            assert_eq!(normalize_trigger(&mut signed, -3.0), 0.0, "clamped");
            let mut fresh = TriggerRange::default();
            assert_eq!(
                normalize_trigger(&mut fresh, f32::NAN),
                0.0,
                "NaN is not a half-pull"
            );
            assert_eq!(fresh, TriggerRange::default(), "NaN does not latch the range");
        }

        /// Latches are per pad and per axis: a −1..1 left trigger does not rescale another
        /// pad's (or the right trigger's) 0..1 values.
        #[test]
        fn trigger_range_latches_are_per_pad_and_axis() {
            let mut ranges = std::collections::BTreeMap::new();
            let left = (1u64, GamepadAxis::LeftTrigger);
            let right = (1u64, GamepadAxis::RightTrigger);
            let other_pad = (2u64, GamepadAxis::LeftTrigger);
            assert_eq!(
                normalize_trigger(ranges.entry(left).or_default(), -1.0),
                0.0,
                "pad 1 left trigger latches as −1..1"
            );
            assert_eq!(normalize_trigger(ranges.entry(left).or_default(), 0.2), 0.6);
            assert_eq!(
                normalize_trigger(ranges.entry(right).or_default(), 0.2),
                0.2,
                "the right trigger of the same pad is a separate latch"
            );
            assert_eq!(
                normalize_trigger(ranges.entry(other_pad).or_default(), 0.2),
                0.2,
                "another pad's trigger is a separate latch"
            );
        }

        /// Regression: the context must be a non-send resource, or `NonSendMut` never sees it and
        /// the backend is silently never polled.
        #[test]
        fn installed_context_is_visible_to_the_poll_system() {
            let mut world = World::new();
            install_context(&mut world, GilrsContext::new(None));
            assert!(world.contains_non_send::<GilrsContext>());
            let seen = world
                .run_system_cached(|context: Option<NonSendMut<GilrsContext>>| context.is_some())
                .unwrap_or(false);
            assert!(seen);
        }
    }
}

#[cfg(all(feature = "devices", not(target_os = "android")))]
pub(crate) use gilrs_backend::{GilrsContext, install_context, poll_gilrs};

/// The desktop backend system's type (fn pointer), for scheduling in the plugin.
#[cfg(all(feature = "devices", not(target_os = "android")))]
pub(crate) type GilrsPoll = fn(
    Option<NonSendMut<GilrsContext>>,
    ResMut<Gamepads>,
    MessageWriter<regia_platform::GamepadConnected>,
    MessageWriter<regia_platform::GamepadDisconnected>,
    MessageWriter<regia_platform::GamepadButtonChanged>,
    MessageWriter<regia_platform::GamepadAxisChanged>,
);

/// The Android backend system's type (fn pointer), for scheduling in the plugin.
#[cfg(target_os = "android")]
pub(crate) type AndroidPoll = fn(
    ResMut<Gamepads>,
    Res<Time>,
    MessageReader<regia_platform::KeyboardInput>,
    MessageReader<regia_platform::TouchInput>,
    Res<crate::Touches>,
    MessageWriter<regia_platform::GamepadConnected>,
    MessageWriter<regia_platform::GamepadDisconnected>,
    MessageWriter<regia_platform::GamepadButtonChanged>,
    MessageWriter<regia_platform::GamepadAxisChanged>,
    Local<AndroidPendingSamples>,
    Local<GamepadScanGate>,
);

// ---------------------------------------------------------------------------
// Android device discovery. The walk of InputManager is a binder call on the
// frame thread, so it runs on a gate the unit tests can count without JNI.
// ---------------------------------------------------------------------------

/// Frames between Android `InputManager` walks while the device set is stable.
///
/// 30 frames re-entered JNI twice a second at 60 Hz. 600 frames is 10 s at 60 Hz
/// and 5 s at 120 Hz. `GamepadScanGate::hint_device_change` walks on the next poll.
#[cfg(any(test, target_os = "android"))]
pub(crate) const ANDROID_GAMEPAD_RESCAN_FRAMES: u64 = 600;

/// Whether this frame should walk `InputManager`.
///
/// `hint` is set when a pad edge arrives and no pad is connected. The fallback
/// interval covers a pad that never produces a button or axis. The Android backend keeps it in a
/// system `Local`, so the hint and the last walk survive whichever thread runs the system (a
/// `thread_local!` lost them under the multi-threaded executor, ADR-4933).
#[cfg(any(test, target_os = "android"))]
#[derive(Debug, Default)]
pub struct GamepadScanGate {
    last_frame: Option<u64>,
    hint: bool,
}

#[cfg(any(test, target_os = "android"))]
impl GamepadScanGate {
    /// The next `due` walks, even inside the fallback interval.
    pub(crate) fn hint_device_change(&mut self) {
        self.hint = true;
    }

    /// `true` when `frame` should call the enumerator. A `true` result records the walk.
    pub(crate) fn due(&mut self, frame: u64) -> bool {
        let elapsed = match self.last_frame {
            None => true,
            // A clock that went backwards is a new window, not a reason to wait.
            Some(last) if frame < last => true,
            Some(last) => frame - last >= ANDROID_GAMEPAD_RESCAN_FRAMES,
        };
        let due = self.hint || elapsed;
        if due {
            self.last_frame = Some(frame);
            self.hint = false;
        }
        due
    }
}

/// Diffs `devices` against `gamepads` and reports connects and disconnects.
///
/// A renamed device is reported as a fresh connect, matching the Android poll. Test stand-in
/// for one enumerate: the Android poll itself plans through `plan_android_devices`.
#[cfg(test)]
pub(crate) fn note_gamepad_scan(
    gamepads: &mut Gamepads,
    devices: impl IntoIterator<Item = (u64, String)>,
    mut connected: impl FnMut(GamepadConnected),
    mut disconnected: impl FnMut(GamepadDisconnected),
) {
    let mut seen: BTreeMap<u64, String> = BTreeMap::new();
    for (id, name) in devices {
        seen.insert(id, name);
    }
    for (id, name) in &seen {
        let pad = gamepads.pads.entry(*id).or_default();
        if pad.name != *name {
            pad.name.clone_from(name);
            connected(GamepadConnected {
                device: *id,
                name: name.clone(),
            });
        }
    }
    for id in gamepads.pads.keys().copied().collect::<Vec<_>>() {
        if !seen.contains_key(&id) && gamepads.pads[&id].is_connected() {
            disconnected(GamepadDisconnected { device: id });
        }
    }
}

// ---------------------------------------------------------------------------
// Android backend: android-activity events seen through winit + InputManager JNI
// ---------------------------------------------------------------------------

#[cfg(target_os = "android")]
mod android_backend {
    use super::*;
    use crate::android_map::{android_keycode, button_from_akeycode, joystick_from_touch};
    use regia_platform::{
        GamepadAxisChanged, GamepadButtonChanged, GamepadConnected, GamepadDisconnected, KeyboardInput,
        TouchInput,
    };

    /// `PreUpdate` (set [`crate::InputSet::Backends`]): the Android gamepad backend.
    ///
    /// * Device discovery: polls `InputManager` through `GamepadScanGate` (JNI) and writes
    ///   `GamepadConnected`/`GamepadDisconnected` for gamepad/joystick-class devices. A button or
    ///   stick edge while no pad is connected hints the next frame to walk again.
    /// * Buttons: winit delivers gamepad keys as `KeyboardInput` with physical key
    ///   `Unidentified(Android(<akeycode>))`. winit does not expose the Android device id, so
    ///   every sample goes to the first pad `InputManager` has actually returned. Samples that
    ///   arrive before that stay unattributed; they are applied on the enumerate frame that
    ///   first reports an id, after that frame's disconnects. An empty enumerate leaves the
    ///   current pads in place.
    /// * Left stick: joystick MotionEvents arrive as touch "Moved" reports (see
    ///   [`joystick_from_touch`]); Android's Y is flipped to up-positive. The right stick and hat
    ///   axes are **not reachable** through winit 0.30 (it reads only AXIS_X/AXIS_Y of the
    ///   pointer) — see the M7 report; full axes need a Java-shim observer hook (specs/21).
    #[tracing::instrument(target = "regia_input", level = "trace", skip_all)]
    #[allow(clippy::too_many_arguments)] // an ECS system: one parameter per resource it touches
    pub fn poll_android_gamepads(
        mut gamepads: ResMut<Gamepads>,
        time: Res<Time>,
        mut keys: MessageReader<KeyboardInput>,
        mut touches: MessageReader<TouchInput>,
        touch_state: Res<crate::Touches>,
        mut connected: MessageWriter<GamepadConnected>,
        mut disconnected: MessageWriter<GamepadDisconnected>,
        mut buttons: MessageWriter<GamepadButtonChanged>,
        mut axes: MessageWriter<GamepadAxisChanged>,
        mut pending: Local<super::AndroidPendingSamples>,
        mut scan_gate: Local<super::GamepadScanGate>,
    ) {
        // --- device discovery -----------------------------------------------------
        let scan = scan_gate.due(time.frame_count());
        let enumerated = if scan {
            Some(
                regia_platform::enumerate_gamepads()
                    .into_iter()
                    .map(|device| (device.id as u64, device.name))
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };
        let plan = super::plan_android_devices(&gamepads, enumerated.as_deref());
        for (device, name) in plan.connect {
            connected.write(GamepadConnected { device, name });
        }
        for device in plan.disconnect {
            disconnected.write(GamepadDisconnected { device });
        }

        // --- buttons and the left stick, attributed after the enumerate plan ------
        // A pad edge while no pad is connected hints the scan gate to walk on the next poll.
        let mut pad_edge_without_a_device = false;
        let mut button_samples = Vec::new();
        for key in keys.read() {
            if let Some(button) = android_keycode(&key.physical_key).and_then(button_from_akeycode) {
                if gamepads.connected() == 0 {
                    pad_edge_without_a_device = true;
                }
                let (state, value) = match key.state {
                    ButtonState::Pressed => (ButtonState::Pressed, 1.0),
                    ButtonState::Released => (ButtonState::Released, 0.0),
                };
                button_samples.push((button, state, value));
            }
        }
        let mut axis_samples = Vec::new();
        for touch in touches.read() {
            if let Some((x, y)) =
                joystick_from_touch(touch.phase, touch.position, touch_state.get(touch.id).is_some())
            {
                if gamepads.connected() == 0 {
                    pad_edge_without_a_device = true;
                }
                axis_samples.push((GamepadAxis::LeftStickX, x));
                axis_samples.push((GamepadAxis::LeftStickY, y));
            }
        }
        let (button_events, axis_events) =
            super::take_android_samples(&mut pending, plan.target, &button_samples, &axis_samples);
        for event in button_events {
            buttons.write(event);
        }
        for event in axis_events {
            axes.write(event);
        }
        if pad_edge_without_a_device {
            scan_gate.hint_device_change();
        }

        // --- rumble ---------------------------------------------------------------
        super::apply_rumble_android(&mut gamepads);
    }
}

#[cfg(target_os = "android")]
pub use android_backend::poll_android_gamepads;

/// Player scale for phone vibration (ADR-3464).
///
/// [`Gamepads::set_rumble_scale`] is the model: `0` turns vibration off, `1` (the default) plays
/// it as requested. Reduced motion does not change this scale. The raw device call stays
/// [`regia_platform::Haptics::vibrate`]; this type only multiplies the amplitude first.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq)]
pub struct PhoneVibration {
    scale: Option<f32>,
}

impl PhoneVibration {
    /// Sets the scale. Clamped into `0..=1`; NaN reads as `1` (full, stored as the default).
    pub fn set_scale(&mut self, scale: f32) {
        let scale = if scale.is_nan() {
            1.0
        } else {
            scale.clamp(0.0, 1.0)
        };
        self.scale = (scale < 1.0).then_some(scale);
    }

    /// The scale in effect (`0..=1`).
    #[must_use]
    pub fn scale(&self) -> f32 {
        self.scale.unwrap_or(1.0)
    }

    /// `amplitude` after the player's scale. A non-finite amplitude reads as `0`.
    #[must_use]
    pub fn scaled_amplitude(&self, amplitude: f32) -> f32 {
        let amplitude = if amplitude.is_finite() {
            amplitude.clamp(0.0, 1.0)
        } else {
            0.0
        };
        amplitude * self.scale()
    }

    /// Vibrates the phone for `duration` at `amplitude` (`0..=1`), scaled by [`Self::scale`].
    /// Desktop builds succeed without moving the device.
    pub fn vibrate(
        &self,
        haptics: &regia_platform::Haptics,
        duration: Duration,
        amplitude: f32,
    ) -> Result<(), regia_platform::ServiceError> {
        haptics.vibrate(duration, self.scaled_amplitude(amplitude))
    }
}

/// Button edges kept while no real device id exists. A missing device must not grow this
/// without bound; older edges are dropped first.
#[cfg(any(target_os = "android", test))]
const MAX_PENDING_ANDROID_BUTTONS: usize = 64;

/// Button and axis samples that arrived before a real Android device id existed.
///
/// The Android poll system keeps one of these in a `Local`. It is not part of the
/// input config or the saved project. Desktop builds do not construct it. Button
/// samples stay in arrival order, so a press and a release in one poll are both edges.
#[cfg(any(target_os = "android", test))]
#[derive(Default)]
pub struct AndroidPendingSamples {
    /// Press and release samples in arrival order. Last-state coalescing drops a click.
    buttons: Vec<(GamepadButton, ButtonState, f32)>,
    axes: BTreeMap<GamepadAxis, f32>,
}

/// What one Android enumerate step should tell [`update_gamepads`].
#[cfg(any(target_os = "android", test))]
struct AndroidDevicePlan {
    connect: Vec<(u64, String)>,
    disconnect: Vec<u64>,
    /// Pad that receives this frame's buttons and axes. `None` until a real id exists.
    target: Option<u64>,
}

/// Connect and disconnect plan for one Android frame.
///
/// `enumerated` is `None` on frames that do not query `InputManager`. `Some` is the query
/// result. An empty list leaves the current pads in place: a failed query and "no devices"
/// are the same `Vec`, and dropping every pad on that result deletes held buttons. A
/// non-empty list connects those ids and disconnects a connected id the list omits.
/// The target is the lowest remaining id. There is no invented device 0.
#[cfg(any(target_os = "android", test))]
fn plan_android_devices(gamepads: &Gamepads, enumerated: Option<&[(u64, String)]>) -> AndroidDevicePlan {
    let mut ids: BTreeSet<u64> = gamepads.pads().map(|(id, _)| id).collect();
    let mut connect = Vec::new();
    let mut disconnect = Vec::new();
    if let Some(found) = enumerated
        && !found.is_empty()
    {
        let seen: BTreeSet<u64> = found.iter().map(|(id, _)| *id).collect();
        for (id, name) in found {
            if gamepads.name(*id) != Some(name.as_str()) {
                connect.push((*id, name.clone()));
            }
            ids.insert(*id);
        }
        for id in gamepads.pads().map(|(id, _)| id).collect::<Vec<_>>() {
            if !seen.contains(&id) {
                disconnect.push(id);
                ids.remove(&id);
            }
        }
    }
    AndroidDevicePlan {
        connect,
        disconnect,
        target: ids.iter().next().copied(),
    }
}

/// Keeps button and axis samples until a real device id exists, then emits them at
/// `target`. Samples that arrive before the first enumerate are not written to device 0.
/// Once a target exists, this frame's samples (and anything still pending) are emitted
/// and the buffer is cleared, so a held button is not repeated on later frames.
#[cfg(any(target_os = "android", test))]
fn take_android_samples(
    pending: &mut AndroidPendingSamples,
    target: Option<u64>,
    buttons: &[(GamepadButton, ButtonState, f32)],
    axes: &[(GamepadAxis, f32)],
) -> (Vec<GamepadButtonChanged>, Vec<GamepadAxisChanged>) {
    for (axis, value) in axes {
        pending.axes.insert(*axis, *value);
    }
    let Some(device) = target else {
        pending.buttons.extend(buttons.iter().copied());
        if pending.buttons.len() > MAX_PENDING_ANDROID_BUTTONS {
            let overflow = pending.buttons.len() - MAX_PENDING_ANDROID_BUTTONS;
            pending.buttons.drain(0..overflow);
        }
        return (Vec::new(), Vec::new());
    };
    let mut button_events = Vec::with_capacity(pending.buttons.len() + buttons.len());
    for (button, state, value) in pending.buttons.drain(..) {
        button_events.push(GamepadButtonChanged {
            device,
            button,
            state,
            value,
        });
    }
    for &(button, state, value) in buttons {
        button_events.push(GamepadButtonChanged {
            device,
            button,
            state,
            value,
        });
    }
    let axis_events = pending
        .axes
        .iter()
        .map(|(axis, value)| GamepadAxisChanged {
            device,
            axis: *axis,
            value: *value,
        })
        .collect();
    pending.axes.clear();
    (button_events, axis_events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::Devices;

    fn app_with_gamepads() -> App {
        let mut app = App::new();
        app.add_message::<GamepadConnected>()
            .add_message::<GamepadDisconnected>()
            .add_message::<GamepadButtonChanged>()
            .add_message::<GamepadAxisChanged>()
            .add_message::<RumbleRequest>()
            .init_resource::<crate::device::LastUsedDevice>()
            .init_resource::<Gamepads>()
            .add_systems(PreUpdate, update_gamepads);
        app
    }

    /// Without a device backend (the `devices`-less server build) nothing drains the rumble
    /// queue; it must stay bounded instead of growing every frame (concurrency audit 11-03 L4).
    #[test]
    fn rumble_queue_stays_bounded_without_a_backend() {
        let mut app = app_with_gamepads();
        for frame in 0..100u64 {
            for i in 0..50u64 {
                app.world_mut().write_message(RumbleRequest {
                    device: frame * 50 + i,
                    strength: 1.0,
                    duration: Duration::from_millis(100),
                });
            }
            app.world_mut()
                .resource_mut::<Gamepads>()
                .rumble(frame, 0.5, Duration::from_millis(50));
            app.update();
        }
        let queued = &app.world().resource::<Gamepads>().rumble;
        assert!(queued.len() <= 64, "{} rumble requests queued", queued.len());
        // The newest requests survive.
        assert_eq!(queued.last().map(|r| r.0), Some(99 * 50 + 49));
    }

    /// Sixty frames of an unchanged device set must walk InputManager once, then once more
    /// when a connect is hinted. The old gate walked on every multiple of 30.
    #[test]
    fn android_gamepad_scan_runs_once_across_sixty_frames_until_a_connect() {
        let mut gate = GamepadScanGate::default();
        let mut gamepads = Gamepads::default();
        let mut walks = 0u32;
        let mut connects = Vec::new();
        for frame in 1..=60 {
            if gate.due(frame) {
                walks += 1;
                note_gamepad_scan(
                    &mut gamepads,
                    [(1u64, "Pad".to_string())],
                    |event| connects.push(event),
                    |_| {},
                );
            }
        }
        assert_eq!(walks, 1, "InputManager walks in 60 stable frames: {walks}");
        assert_eq!(gamepads.connected(), 1);
        assert_eq!(connects.len(), 1);

        gate.hint_device_change();
        let mut hinted = 0u32;
        if gate.due(61) {
            hinted += 1;
            note_gamepad_scan(
                &mut gamepads,
                [(1u64, "Pad".to_string()), (2, "Second".to_string())],
                |event| connects.push(event),
                |_| {},
            );
        }
        assert_eq!(
            hinted, 1,
            "a connect hint must walk again before the fallback interval"
        );
        assert_eq!(gamepads.connected(), 2);
        assert_eq!(gamepads.name(2), Some("Second"));
        // The pad that was already listed is not reported again.
        assert_eq!(connects.len(), 2);
    }

    /// Frame 31 is not a multiple of 30. Only a connect hint walks there.
    #[test]
    fn a_connect_hint_walks_input_manager_before_the_fallback() {
        let mut gate = GamepadScanGate::default();
        assert!(gate.due(30), "the first walk is due");
        gate.hint_device_change();
        assert!(
            gate.due(31),
            "frame 31 is inside the old 30-frame step; only a connect hint walks again"
        );
        assert!(
            !gate.due(32),
            "the hint is one walk, then the fallback interval applies"
        );
    }

    /// The fallback is 600 frames, not the old 30-frame step. A clock that jumps backwards walks.
    #[test]
    fn the_fallback_walk_is_six_hundred_frames_and_a_backwards_clock_walks() {
        let mut gate = GamepadScanGate::default();
        assert!(gate.due(1), "the first poll walks");
        assert!(!gate.due(30), "frame 30 is no longer a walk");
        assert!(
            !gate.due(600),
            "599 frames after frame 1 is still inside the window"
        );
        assert!(gate.due(601), "600 frames after frame 1 walks again");
        assert!(
            gate.due(100),
            "a frame clock that went backwards starts a new window"
        );
        assert!(
            !gate.due(699),
            "the new window is measured from the backwards walk"
        );
        assert!(gate.due(700));
    }

    #[test]
    fn connection_and_button_lifecycle_from_messages() {
        let mut app = app_with_gamepads();
        app.world_mut().write_message(GamepadConnected {
            device: 7,
            name: "Test Pad".into(),
        });
        app.update();

        let gamepads = app.world().resource::<Gamepads>();
        assert_eq!(gamepads.connected(), 1);
        assert_eq!(gamepads.name(7), Some("Test Pad"));

        app.world_mut().write_message(GamepadButtonChanged {
            device: 7,
            button: GamepadButton::South,
            state: ButtonState::Pressed,
            value: 1.0,
        });
        app.world_mut().write_message(GamepadAxisChanged {
            device: 7,
            axis: GamepadAxis::LeftStickY,
            value: -0.75,
        });
        app.update();
        {
            let gamepads = app.world().resource::<Gamepads>();
            assert!(gamepads.pressed(7, GamepadButton::South));
            assert_eq!(gamepads.just_pressed(GamepadButton::South), Some(7));
            assert_eq!(gamepads.axis(7, GamepadAxis::LeftStickY), -0.75);
        }

        // Analog trigger edge at the threshold; edges reset the next frame.
        app.world_mut().write_message(GamepadButtonChanged {
            device: 7,
            button: GamepadButton::LeftTrigger,
            state: ButtonState::Pressed,
            value: 0.4,
        });
        app.update();
        {
            let gamepads = app.world().resource::<Gamepads>();
            assert!(
                !gamepads.pressed(7, GamepadButton::LeftTrigger),
                "0.4 < threshold"
            );
            assert_eq!(
                gamepads
                    .pad(7)
                    .map(|p| p.button_value(GamepadButton::LeftTrigger)),
                Some(0.4),
                "analog value preserved even below the threshold"
            );
            assert_eq!(gamepads.just_pressed(GamepadButton::South), None, "edge cleared");
        }

        app.world_mut().write_message(GamepadDisconnected { device: 7 });
        app.update();
        let gamepads = app.world().resource::<Gamepads>();
        assert_eq!(gamepads.connected(), 0);
        assert!(gamepads.pad(7).is_none());
        assert!(gamepads.pads.is_empty(), "disconnect removes the map entry");
    }

    #[test]
    fn devices_view_combines_pads_and_respects_filters() {
        let mut gamepads = Gamepads::default();
        let mut pad = Pad {
            name: "p".into(),
            ..Default::default()
        };
        pad.buttons.insert(
            GamepadButton::LeftTrigger,
            PadButton {
                value: 0.7,
                ..Default::default()
            },
        );
        gamepads.pads.insert(1, pad);
        let (keyboard, mouse) = (
            crate::device::Keyboard::default(),
            crate::device::Mouse::default(),
        );
        let devices = Devices::new(&keyboard, &mouse, &gamepads);
        assert!((devices.max_button_value(GamepadButton::LeftTrigger) - 0.7).abs() < 1e-6);
        let filtered = Devices {
            pad: Some(2),
            ..devices
        };
        assert_eq!(filtered.max_button_value(GamepadButton::LeftTrigger), 0.0);
    }

    #[test]
    fn devices_axis_prefers_the_pad_with_the_largest_magnitude() {
        let mut gamepads = Gamepads::default();
        for (id, value) in [(0u64, 0.0f32), (1, -0.6)] {
            // A pad with a name is connected; `pads()` skips unnamed entries.
            let mut pad = Pad {
                name: format!("pad {id}"),
                ..Pad::default()
            };
            pad.axes.insert(GamepadAxis::LeftStickX, value);
            gamepads.pads.insert(id, pad);
        }
        let (keyboard, mouse) = (
            crate::device::Keyboard::default(),
            crate::device::Mouse::default(),
        );
        let devices = Devices::new(&keyboard, &mouse, &gamepads);
        assert!((devices.axis(GamepadAxis::LeftStickX) + 0.6).abs() < 1e-6);
        let only_idle = Devices {
            pad: Some(0),
            ..devices
        };
        assert_eq!(only_idle.axis(GamepadAxis::LeftStickX), 0.0);
    }

    #[test]
    fn analog_edges_clamp_rumble_and_the_first_pad() {
        let mut app = app_with_gamepads();
        assert_eq!(app.world().resource::<Gamepads>().first(), None);
        assert_eq!(
            app.world()
                .resource::<Gamepads>()
                .just_released(GamepadButton::South),
            None
        );

        // A button or axis before Connected still creates a usable pad, with a placeholder name,
        // and out-of-range magnitudes are clamped into the canonical ranges.
        app.world_mut().write_message(GamepadButtonChanged {
            device: 4,
            button: GamepadButton::South,
            state: ButtonState::Pressed,
            value: 1.5,
        });
        app.world_mut().write_message(GamepadAxisChanged {
            device: 5,
            axis: GamepadAxis::LeftStickY,
            value: -2.0,
        });
        app.update();
        {
            let gamepads = app.world().resource::<Gamepads>();
            assert_eq!(gamepads.name(4), Some("gamepad"));
            assert_eq!(
                gamepads.pad(4).map(|pad| pad.button_value(GamepadButton::South)),
                Some(1.0),
                "button values clamp to 1"
            );
            assert!(gamepads.pressed(4, GamepadButton::South));
            assert_eq!(gamepads.name(5), Some("gamepad"));
            assert_eq!(
                gamepads.axis(5, GamepadAxis::LeftStickY),
                -1.0,
                "axes clamp to -1"
            );
            assert_eq!(gamepads.first(), Some(4), "ids are ordered");
        }

        app.world_mut().write_message(GamepadConnected {
            device: 4,
            name: "Renamed".into(),
        });
        app.world_mut().write_message(GamepadConnected {
            device: 2,
            name: "Low".into(),
        });
        app.update();
        assert_eq!(app.world().resource::<Gamepads>().name(4), Some("Renamed"));
        assert_eq!(app.world().resource::<Gamepads>().first(), Some(2));

        // The threshold is exclusive: 0.5 is still released, and dropping onto it is a release.
        app.world_mut().write_message(GamepadButtonChanged {
            device: 2,
            button: GamepadButton::South,
            state: ButtonState::Pressed,
            value: 0.5,
        });
        app.update();
        {
            let gamepads = app.world().resource::<Gamepads>();
            assert!(!gamepads.pressed(2, GamepadButton::South));
            assert_eq!(
                gamepads.pad(2).map(|pad| pad.button_value(GamepadButton::South)),
                Some(0.5)
            );
            assert!(gamepads.pressed(4, GamepadButton::South), "pad 4 stays down");
            assert_eq!(
                gamepads.just_pressed(GamepadButton::South),
                None,
                "the edge from the earlier frame was cleared"
            );
            assert_eq!(gamepads.just_released(GamepadButton::South), None);
        }
        app.world_mut().write_message(GamepadButtonChanged {
            device: 2,
            button: GamepadButton::South,
            state: ButtonState::Pressed,
            value: 0.75,
        });
        app.update();
        assert_eq!(
            app.world()
                .resource::<Gamepads>()
                .just_pressed(GamepadButton::South),
            Some(2)
        );
        app.world_mut().write_message(GamepadButtonChanged {
            device: 2,
            button: GamepadButton::South,
            state: ButtonState::Released,
            value: 0.5,
        });
        app.world_mut().write_message(GamepadButtonChanged {
            device: 4,
            button: GamepadButton::South,
            state: ButtonState::Released,
            value: -0.25,
        });
        app.update();
        {
            let gamepads = app.world().resource::<Gamepads>();
            assert!(!gamepads.pressed(2, GamepadButton::South));
            assert!(!gamepads.pressed(4, GamepadButton::South));
            assert_eq!(
                gamepads.pad(4).map(|pad| pad.button_value(GamepadButton::South)),
                Some(0.0),
                "negative button values clamp to 0"
            );
            assert_eq!(
                gamepads.just_released(GamepadButton::South),
                Some(2),
                "the lower id is reported when several pads release together"
            );
        }

        // Press and release in one frame record both edges; the stored value is the last event.
        app.world_mut().write_message(GamepadButtonChanged {
            device: 2,
            button: GamepadButton::East,
            state: ButtonState::Pressed,
            value: 1.0,
        });
        app.world_mut().write_message(GamepadButtonChanged {
            device: 2,
            button: GamepadButton::East,
            state: ButtonState::Released,
            value: 0.0,
        });
        app.world_mut().write_message(GamepadAxisChanged {
            device: 2,
            axis: GamepadAxis::RightStickX,
            value: 1.5,
        });
        app.update();
        {
            let gamepads = app.world().resource::<Gamepads>();
            let east = gamepads
                .pad(2)
                .and_then(|pad| pad.buttons.get(&GamepadButton::East))
                .copied()
                .expect("east was reported");
            assert!(east.just_pressed && east.just_released);
            assert_eq!(east.value, 0.0);
            assert!(!gamepads.pressed(2, GamepadButton::East));
            assert_eq!(gamepads.just_pressed(GamepadButton::East), Some(2));
            assert_eq!(gamepads.just_released(GamepadButton::East), Some(2));
            assert_eq!(gamepads.axis(2, GamepadAxis::RightStickX), 1.0);
        }

        app.world_mut().write_message(GamepadDisconnected { device: 2 });
        app.world_mut().write_message(GamepadDisconnected { device: 99 });
        app.update();
        assert_eq!(app.world().resource::<Gamepads>().first(), Some(4));
        assert!(app.world().resource::<Gamepads>().pad(2).is_none());

        app.world_mut()
            .resource_mut::<Gamepads>()
            .rumble(4, 1.5, Duration::from_millis(30));
        app.world_mut()
            .resource_mut::<Gamepads>()
            .rumble(4, -0.2, Duration::from_secs(1));
        app.world_mut().write_message(RumbleRequest {
            device: 4,
            strength: 0.25,
            duration: Duration::from_millis(5),
        });
        app.update();
        assert_eq!(
            app.world().resource::<Gamepads>().rumble,
            vec![
                (4, 1.0, Duration::from_millis(30)),
                (4, 0.0, Duration::from_secs(1)),
                (4, 0.25, Duration::from_millis(5)),
            ]
        );
    }

    #[test]
    fn the_rumble_scale_turns_every_request_down_or_off() {
        let mut app = app_with_gamepads();
        assert_eq!(app.world().resource::<Gamepads>().rumble_scale(), 1.0);
        app.world_mut().resource_mut::<Gamepads>().set_rumble_scale(0.5);
        app.world_mut()
            .resource_mut::<Gamepads>()
            .rumble(1, 0.8, Duration::from_millis(10));
        app.world_mut().write_message(RumbleRequest {
            device: 1,
            strength: 0.5,
            duration: Duration::from_millis(20),
        });
        app.update();
        assert_eq!(
            std::mem::take(&mut app.world_mut().resource_mut::<Gamepads>().rumble),
            vec![
                (1, 0.4, Duration::from_millis(10)),
                (1, 0.25, Duration::from_millis(20)),
            ]
        );
        // Off: requests still queue (a stop is a stop) but at zero strength.
        let mut pads = app.world_mut().resource_mut::<Gamepads>();
        pads.set_rumble_scale(0.0);
        pads.rumble(1, 1.0, Duration::from_millis(10));
        assert_eq!(pads.rumble, vec![(1, 0.0, Duration::from_millis(10))]);
        // Out-of-range and NaN values clamp; 1 is the default again.
        pads.set_rumble_scale(7.0);
        assert_eq!(pads.rumble_scale(), 1.0);
        pads.set_rumble_scale(f32::NAN);
        assert_eq!(pads.rumble_scale(), 1.0);
        pads.set_rumble_scale(-1.0);
        assert_eq!(pads.rumble_scale(), 0.0);
    }

    #[test]
    fn phone_vibration_scales_amplitude_and_desktop_accepts_it() {
        let mut phone = PhoneVibration::default();
        assert_eq!(phone.scale(), 1.0);
        assert_eq!(phone.scaled_amplitude(0.5), 0.5);
        phone.set_scale(0.5);
        assert_eq!(phone.scale(), 0.5);
        assert_eq!(phone.scaled_amplitude(1.0), 0.5);
        phone.set_scale(0.0);
        assert_eq!(phone.scaled_amplitude(1.0), 0.0);
        phone.set_scale(f32::NAN);
        assert_eq!(phone.scale(), 1.0);
        phone.set_scale(2.0);
        assert_eq!(phone.scale(), 1.0);
        phone.set_scale(-1.0);
        assert_eq!(phone.scale(), 0.0);
        phone.set_scale(0.5);
        let haptics = regia_platform::Haptics::default();
        assert!(phone.vibrate(&haptics, Duration::from_millis(10), 1.0).is_ok());
    }

    /// Drives the Android attribution helpers the way `poll_android_gamepads` does:
    /// connect and disconnect messages first, then this frame's buttons, then
    /// [`update_gamepads`].
    fn dispatch_android_frame(
        app: &mut App,
        pending: &mut AndroidPendingSamples,
        enumerated: Option<Vec<(u64, String)>>,
        buttons: &[(GamepadButton, ButtonState, f32)],
    ) {
        let plan = plan_android_devices(app.world().resource::<Gamepads>(), enumerated.as_deref());
        for (device, name) in plan.connect {
            app.world_mut().write_message(GamepadConnected { device, name });
        }
        for device in plan.disconnect {
            app.world_mut().write_message(GamepadDisconnected { device });
        }
        let (button_events, axis_events) = take_android_samples(pending, plan.target, buttons, &[]);
        for event in button_events {
            app.world_mut().write_message(event);
        }
        for event in axis_events {
            app.world_mut().write_message(event);
        }
        app.update();
    }

    #[test]
    fn android_buttons_wait_for_a_real_device_and_survive_an_empty_enumerate() {
        let mut app = app_with_gamepads();
        let mut pending = AndroidPendingSamples::default();

        // Frame 1 is not an enumerate frame. South is held. Nothing may land on
        // an invented device 0.
        dispatch_android_frame(
            &mut app,
            &mut pending,
            None,
            &[(GamepadButton::South, ButtonState::Pressed, 1.0)],
        );
        {
            let pads = app.world().resource::<Gamepads>();
            assert!(pads.pad(0).is_none(), "no invented device 0");
            assert_eq!(pads.connected(), 0);
        }

        // Frame 30: device 3 is enumerated. winit does not repeat the button down.
        // The held sample must be down on id 3.
        dispatch_android_frame(&mut app, &mut pending, Some(vec![(3, "Pad".into())]), &[]);
        {
            let pads = app.world().resource::<Gamepads>();
            assert!(pads.pad(0).is_none(), "device 0 was never a pad");
            assert!(
                pads.pressed(3, GamepadButton::South),
                "the button held since before enumerate is down on device 3"
            );
            assert_eq!(pads.name(3), Some("Pad"));
        }

        // An empty enumerate (JNI failure, or no devices reported) leaves the pad
        // and its held button in place.
        dispatch_android_frame(&mut app, &mut pending, Some(vec![]), &[]);
        {
            let pads = app.world().resource::<Gamepads>();
            assert_eq!(pads.connected(), 1);
            assert!(pads.pressed(3, GamepadButton::South));
            assert_eq!(pads.name(3), Some("Pad"));
        }

        // A later non-empty enumerate that omits 3 and lists 5 disconnects 3.
        dispatch_android_frame(&mut app, &mut pending, Some(vec![(5, "Other".into())]), &[]);
        {
            let pads = app.world().resource::<Gamepads>();
            assert!(pads.pad(3).is_none(), "a listed set that omits 3 disconnects it");
            assert_eq!(pads.name(5), Some("Other"));
        }
    }

    /// A press and a release in one poll are two edges. Coalescing to the last state drops
    /// the click: `update_gamepads` only keeps `just_pressed` when it sees the press.
    #[test]
    fn audfix_android_press_and_release_keeps_both_edges() {
        let mut pending = AndroidPendingSamples::default();
        let (buttons, _) = take_android_samples(
            &mut pending,
            Some(3),
            &[
                (GamepadButton::South, ButtonState::Pressed, 1.0),
                (GamepadButton::South, ButtonState::Released, 0.0),
            ],
            &[],
        );
        assert_eq!(
            buttons.len(),
            2,
            "press then release must both be emitted, got {buttons:?}"
        );
        assert_eq!(buttons[0].state, ButtonState::Pressed);
        assert_eq!(buttons[0].value, 1.0);
        assert_eq!(buttons[1].state, ButtonState::Released);
        assert_eq!(buttons[1].value, 0.0);
        assert!(
            pending.buttons.is_empty(),
            "a known device does not keep the edges"
        );
    }

    /// A press and a release in one Android batch must both reach [`update_gamepads`].
    /// Collapsing them to the last sample drops the tap: the button was never down, so
    /// the release is not an edge either.
    #[test]
    fn android_same_frame_press_and_release_keeps_both_edges() {
        let mut app = app_with_gamepads();
        let mut pending = AndroidPendingSamples::default();
        dispatch_android_frame(&mut app, &mut pending, Some(vec![(3, "Pad".into())]), &[]);
        dispatch_android_frame(
            &mut app,
            &mut pending,
            None,
            &[
                (GamepadButton::East, ButtonState::Pressed, 1.0),
                (GamepadButton::East, ButtonState::Released, 0.0),
            ],
        );
        {
            let pads = app.world().resource::<Gamepads>();
            let east = pads
                .pad(3)
                .and_then(|pad| pad.buttons.get(&GamepadButton::East))
                .copied()
                .expect("east was reported");
            assert!(
                east.just_pressed && east.just_released,
                "a same-frame tap keeps both edges, value {east:?}"
            );
            assert_eq!(east.value, 0.0);
            assert!(!pads.pressed(3, GamepadButton::East));
        }
        dispatch_android_frame(&mut app, &mut pending, None, &[]);
        let pads = app.world().resource::<Gamepads>();
        assert_eq!(pads.just_pressed(GamepadButton::East), None);
        assert_eq!(pads.just_released(GamepadButton::East), None);
    }
}
