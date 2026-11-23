// SPDX-License-Identifier: MPL-2.0
//! [`SpaceMousePlugin`]: the resources, messages and `PreUpdate` system of the SpaceMouse
//! (`docs/specs/29-spacemouse.md` §§ 2.5–3). Added by [`crate::InputPlugin`] on desktop; it does
//! nothing until a device appears.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use regia_app::prelude::*;

use super::backend::{BackendEvent, HidBackend, SpaceMouseBackend, SpaceMouseDeviceInfo, SpaceMouseWaker};
use super::{SpaceMouseNav, SpaceMouseState, actions, button_action};
use crate::config::{InputConfig, SpaceMouseConfig};

/// The udev rule that gives the logged-in user read access to SpaceMouse hidraw nodes on Linux
/// (`book/user-guide/spacemouse.md`). Logged once when a device is refused for permissions.
pub const LINUX_UDEV_RULE: &str = "\
# /etc/udev/rules.d/70-spacemouse.rules: read access to SpaceMouse HID nodes for the logged-in user
KERNEL==\"hidraw*\", ATTRS{idVendor}==\"256f\", MODE=\"0660\", TAG+=\"uaccess\"
KERNEL==\"hidraw*\", ATTRS{idVendor}==\"046d\", ATTRS{idProduct}==\"c6[0-2][0-9a-f]\", MODE=\"0660\", TAG+=\"uaccess\"";

/// A device button changed (§ 3). `button` is 1-based.
///
/// Edges describe the merged [`SpaceMouseState::buttons`] (the OR of every connected
/// device): a press is emitted when any device newly pressed the button, and a release only
/// when the merged state actually cleared the bit — one device tapping a button that another
/// device holds sends the press but not a bogus release (fixrev review, finding 5). The
/// message carries no device id; consumers that need per-device attribution read the backend
/// events instead.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceMouseButton {
    /// The button, 1-based (bit `button - 1` of [`SpaceMouseState::buttons`]).
    pub button: u8,
    /// True on press, false on release.
    pub pressed: bool,
}

/// A device button was pressed and its action (§ 2.5) fired: `view.fit_selection`,
/// `view.home`, `spacemouse.dominant_toggle`, `spacemouse.horizon_lock_toggle` or
/// `spacemouse.button.N`. Apps implement the two `view.*` actions; the toggles are applied by
/// the plugin to [`SpaceMouseNav`] before this is sent.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct SpaceMouseAction {
    /// The action name.
    pub action: String,
    /// The button that fired it.
    pub button: u8,
}

/// A SpaceMouse was opened.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct SpaceMouseConnected {
    /// The device.
    pub device: SpaceMouseDeviceInfo,
}

/// A SpaceMouse went away.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceMouseDisconnected {
    /// The id from [`SpaceMouseConnected`].
    pub id: u64,
}

/// The state of one known device in [`SpaceMouseDevices`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SpaceMouseDeviceStatus {
    /// Open and sending samples.
    Connected,
    /// Present but not readable (§ 2.6): permission, or owned by another driver.
    Unavailable(String),
}

/// One device known to the plugin.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpaceMouseDevice {
    /// Identity (id 0 for an unavailable device, which was never opened).
    pub info: SpaceMouseDeviceInfo,
    /// Connected or unavailable.
    pub status: SpaceMouseDeviceStatus,
}

/// Every SpaceMouse the backend knows about (§ 3), for status displays and the agent surface.
#[derive(Resource, Debug, Clone, Default)]
pub struct SpaceMouseDevices {
    /// Connected devices first, then unavailable ones.
    pub devices: Vec<SpaceMouseDevice>,
    /// The active backend (`"hid"`, `"fake"`), `None` before it starts or when disabled.
    pub backend: Option<&'static str>,
    states: BTreeMap<u64, SpaceMouseState>,
    warned_permission: bool,
}

impl SpaceMouseDevices {
    /// How many devices are connected.
    pub fn connected(&self) -> usize {
        self.devices
            .iter()
            .filter(|d| d.status == SpaceMouseDeviceStatus::Connected)
            .count()
    }
}

/// Programmatic SpaceMouse input (`input.spacemouse_inject`, tests): the injected state
/// replaces the device state for the next `frames` frames (§ 4).
#[derive(Resource, Debug, Clone, Default)]
pub struct SpaceMouseInjection {
    state: Option<SpaceMouseState>,
    frames: u32,
}

impl SpaceMouseInjection {
    /// Replaces the device state with `state` for the next `frames` frames (at least one).
    pub fn inject(&mut self, state: SpaceMouseState, frames: u32) {
        self.state = Some(state);
        self.frames = frames.max(1);
    }

    /// Frames of injected input still to come.
    pub fn remaining(&self) -> u32 {
        self.frames
    }

    /// Takes this frame's injected state, if any.
    fn take_frame(&mut self) -> Option<SpaceMouseState> {
        if self.frames == 0 {
            self.state = None;
            return None;
        }
        self.frames -= 1;
        self.state
    }
}

/// Holds the backend the plugin polls. Insert `SpaceMouseBackendSlot::new(..)` to replace it
/// (tests use [`super::FakeSpaceMouse`]); the default slot starts the HID backend on the first
/// frame, unless the config disables the device.
#[derive(Resource)]
pub struct SpaceMouseBackendSlot {
    backend: Mutex<Option<Box<dyn SpaceMouseBackend>>>,
    auto_start: bool,
}

impl SpaceMouseBackendSlot {
    /// A slot with `backend` installed.
    pub fn new(backend: Box<dyn SpaceMouseBackend>) -> Self {
        Self {
            backend: Mutex::new(Some(backend)),
            auto_start: false,
        }
    }

    /// A slot that starts the OS HID backend when first needed.
    pub fn hid() -> Self {
        Self {
            backend: Mutex::new(None),
            auto_start: true,
        }
    }
}

/// Installs the SpaceMouse resources and the `PreUpdate` update (§ 3). [`crate::InputPlugin`]
/// adds it on desktop; add it yourself first only to pass a different backend.
pub struct SpaceMousePlugin {
    backend: Mutex<Option<Box<dyn SpaceMouseBackend>>>,
}

impl Default for SpaceMousePlugin {
    /// The OS HID backend.
    fn default() -> Self {
        Self {
            backend: Mutex::new(None),
        }
    }
}

impl SpaceMousePlugin {
    /// The plugin with a custom backend (e.g. [`super::FakeSpaceMouse::backend`]).
    pub fn with_backend(backend: Box<dyn SpaceMouseBackend>) -> Self {
        Self {
            backend: Mutex::new(Some(backend)),
        }
    }
}

impl Plugin for SpaceMousePlugin {
    fn build(&self, app: &mut App) {
        let slot = match self.backend.lock().unwrap_or_else(PoisonError::into_inner).take() {
            Some(backend) => SpaceMouseBackendSlot::new(backend),
            None => SpaceMouseBackendSlot::hid(),
        };
        app.add_message::<SpaceMouseButton>()
            .add_message::<SpaceMouseAction>()
            .add_message::<SpaceMouseConnected>()
            .add_message::<SpaceMouseDisconnected>()
            .init_resource::<SpaceMouseState>()
            .init_resource::<SpaceMouseNav>()
            .init_resource::<SpaceMouseDevices>()
            .init_resource::<SpaceMouseInjection>()
            .insert_resource(slot)
            .add_systems(PreUpdate, update_spacemouse.in_set(crate::InputSet::Devices));
    }
}

/// `PreUpdate` (set [`crate::InputSet::Devices`]): drains the backend (starting the HID
/// backend on first use), keeps the per-device states, merges them into [`SpaceMouseState`]
/// (injected input replaces them), applies `enabled`, and turns button edges into
/// [`SpaceMouseButton`] / [`SpaceMouseAction`] messages, flipping [`SpaceMouseNav`] on the
/// toggle buttons.
#[allow(clippy::too_many_arguments)] // one system per frame; the params are its whole interface
#[tracing::instrument(target = "regia_input", level = "trace", skip_all)]
pub fn update_spacemouse(
    slot: Res<SpaceMouseBackendSlot>,
    waker: Option<Res<SpaceMouseWaker>>,
    config: Option<Res<InputConfig>>,
    mut devices: ResMut<SpaceMouseDevices>,
    mut state: ResMut<SpaceMouseState>,
    mut nav: ResMut<SpaceMouseNav>,
    mut injection: ResMut<SpaceMouseInjection>,
    mut buttons: MessageWriter<SpaceMouseButton>,
    mut actions_out: MessageWriter<SpaceMouseAction>,
    mut connected: MessageWriter<SpaceMouseConnected>,
    mut disconnected: MessageWriter<SpaceMouseDisconnected>,
) {
    let config = config.map_or(SpaceMouseConfig::DEFAULT, |c| c.spacemouse.sanitized());
    let mut events = Vec::new();
    {
        let mut backend = slot.backend.lock().unwrap_or_else(PoisonError::into_inner);
        if backend.is_none() && slot.auto_start && config.enabled {
            *backend = HidBackend::start(waker.map(|w| w.clone()))
                .map(|b| Box::new(b) as Box<dyn SpaceMouseBackend>);
        }
        if let Some(backend) = backend.as_mut() {
            devices.backend = Some(backend.name());
            backend.poll(&mut events);
        }
    }
    // Button edges: each device's sample is diffed against that device's own previous sample
    // (held in `devices.states` until the event is applied), and the per-device changes are
    // OR-ed into one "newly pressed" and one "newly released" mask for the whole poll. The
    // masks do not depend on which device thread delivered first (audit finding 8, ADR-4768),
    // and the emitted messages describe the merged [`SpaceMouseState::buttons`]: a press when
    // any device newly pressed the bit, a release only when the merged state actually cleared
    // it — one device tapping a button another device holds no longer sends a release the
    // merged state contradicts (fixrev review, finding 5). A press and a release of one
    // device inside one poll still produce both edges, as before (concurrency audit 11-03 L2).
    let mut newly_pressed: u32 = 0;
    let mut newly_released: u32 = 0;
    for event in events {
        let diff = match &event {
            BackendEvent::Sample { id, state } => {
                Some((devices.states.get(id).map_or(0, |s| s.buttons), state.buttons))
            }
            BackendEvent::Disconnected { id } => Some((devices.states.get(id).map_or(0, |s| s.buttons), 0)),
            _ => None,
        };
        apply_event(&mut devices, event, &mut connected, &mut disconnected);
        if let Some((before, after)) = diff {
            newly_pressed |= after & !before;
            newly_released |= before & !after;
        }
    }

    let previous = state.buttons;
    let (next, from_devices) = match injection.take_frame() {
        Some(injected) => (injected, false),
        None if config.enabled => (SpaceMouseState::merged(devices.states.values()), true),
        None => (SpaceMouseState::default(), false),
    };
    if *state != next {
        *state = next;
    }

    if from_devices {
        // Releases the merged state still holds (another device keeps the button down) are
        // not emitted: the message must not contradict [`SpaceMouseState::buttons`]. The
        // toggle flip keys on the merged *rising* edge, so it cannot fire for an edge the
        // merged state never had.
        emit_button_edges(
            newly_pressed,
            newly_released & !next.buttons,
            next.buttons & !previous,
            &mut nav,
            &mut buttons,
            &mut actions_out,
        );
    } else {
        // Injected input replaces the devices, and a disabled device reports nothing: those
        // diff once against the final merged state.
        emit_button_edges(
            next.buttons & !previous,
            previous & !next.buttons,
            next.buttons & !previous,
            &mut nav,
            &mut buttons,
            &mut actions_out,
        );
    }
}

/// Emits [`SpaceMouseButton`] / [`SpaceMouseAction`] messages for the buttons in `pressed`
/// and `released`, and flips the [`SpaceMouseNav`] toggles on the buttons in `toggles` (the
/// merged rising edges of this poll): at most one toggle per button per poll, delivery-order
/// independent, and never for a per-device edge the merged state never had (fixrev review,
/// finding 5). Presses fire their actions even without a merged rising edge: a tap inside one
/// poll is a real press and must reach its action (concurrency audit 11-03 L2).
fn emit_button_edges(
    pressed: u32,
    released: u32,
    toggles: u32,
    nav: &mut SpaceMouseNav,
    buttons: &mut MessageWriter<SpaceMouseButton>,
    actions_out: &mut MessageWriter<SpaceMouseAction>,
) {
    for bit in 0..32u8 {
        let mask = 1 << bit;
        let press = pressed & mask != 0;
        let release = released & mask != 0;
        if !press && !release {
            continue;
        }
        let button = bit + 1;
        if press {
            buttons.write(SpaceMouseButton {
                button,
                pressed: true,
            });
            let action = button_action(button);
            if toggles & mask != 0 {
                match action.as_str() {
                    actions::DOMINANT_TOGGLE => nav.dominant = !nav.dominant,
                    actions::HORIZON_LOCK_TOGGLE => nav.horizon_lock = !nav.horizon_lock,
                    _ => {}
                }
            }
            actions_out.write(SpaceMouseAction { action, button });
        }
        if release {
            buttons.write(SpaceMouseButton {
                button,
                pressed: false,
            });
        }
    }
}

fn apply_event(
    devices: &mut SpaceMouseDevices,
    event: BackendEvent,
    connected: &mut MessageWriter<SpaceMouseConnected>,
    disconnected: &mut MessageWriter<SpaceMouseDisconnected>,
) {
    match event {
        BackendEvent::Connected(info) => {
            tracing::info!(target: "regia_input", "SpaceMouse connected: {} ({:04x}:{:04x})",
                info.name, info.vendor_id, info.product_id);
            // A device that opened is no longer unavailable.
            devices.devices.retain(|d| {
                !(matches!(d.status, SpaceMouseDeviceStatus::Unavailable(_)) && d.info.name == info.name)
            });
            devices.states.insert(info.id, SpaceMouseState::default());
            devices.devices.insert(
                devices.connected(),
                SpaceMouseDevice {
                    info: info.clone(),
                    status: SpaceMouseDeviceStatus::Connected,
                },
            );
            connected.write(SpaceMouseConnected { device: info });
        }
        BackendEvent::Disconnected { id } => {
            tracing::info!(target: "regia_input", "SpaceMouse {id} disconnected");
            devices.states.remove(&id);
            let before = devices.devices.len();
            devices
                .devices
                .retain(|d| !(d.info.id == id && d.status == SpaceMouseDeviceStatus::Connected));
            if devices.devices.len() < before {
                disconnected.write(SpaceMouseDisconnected { id });
            }
        }
        BackendEvent::Sample { id, state } => {
            if let Some(slot) = devices.states.get_mut(&id) {
                // Latest sample wins (§ 2.2: never sum events).
                *slot = state;
            }
        }
        BackendEvent::Unavailable {
            name,
            reason,
            permission,
        } => {
            if permission && !devices.warned_permission {
                devices.warned_permission = true;
                tracing::warn!(
                    target: "regia_input",
                    "SpaceMouse {name:?} found but not readable ({reason}). Install this udev rule, then \
                     run `sudo udevadm control --reload-rules && sudo udevadm trigger` and replug:\n{LINUX_UDEV_RULE}"
                );
            } else {
                tracing::debug!(target: "regia_input", "SpaceMouse {name:?} unavailable: {reason}");
            }
            let entry = SpaceMouseDevice {
                info: SpaceMouseDeviceInfo {
                    id: 0,
                    name,
                    vendor_id: 0,
                    product_id: 0,
                    source: "hid".to_string(),
                },
                status: SpaceMouseDeviceStatus::Unavailable(reason),
            };
            if !devices.devices.contains(&entry) {
                devices.devices.push(entry);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spacemouse::backend::FakeSpaceMouse;

    fn app(fake: &FakeSpaceMouse) -> App {
        let mut app = App::new();
        app.add_plugins(SpaceMousePlugin::with_backend(fake.backend()));
        app.add_plugins(crate::InputPlugin::default());
        app
    }

    fn drain<M: Message + Clone>(app: &mut App) -> Vec<M> {
        app.world_mut().resource_mut::<Messages<M>>().drain().collect()
    }

    /// A button tap that starts and ends between two frames (a hitch, a slow frame) still
    /// reports its press and release, like keys and gamepad buttons (concurrency audit 11-03 L2).
    #[test]
    fn a_tap_inside_one_poll_still_reports_both_edges() {
        let fake = FakeSpaceMouse::default();
        let mut app = app(&fake);
        fake.connect(3, "SpaceMouse Compact");
        app.update();
        let _ = drain::<SpaceMouseButton>(&mut app);
        fake.sample(
            3,
            SpaceMouseState {
                buttons: 0b1,
                ..Default::default()
            },
        );
        fake.sample(3, SpaceMouseState::default());
        app.update();
        let edges: Vec<(u8, bool)> = drain::<SpaceMouseButton>(&mut app)
            .into_iter()
            .map(|b| (b.button, b.pressed))
            .collect();
        assert_eq!(edges, vec![(1, true), (1, false)], "the tap was invisible");
        assert_eq!(
            drain::<SpaceMouseAction>(&mut app).len(),
            1,
            "the press fires its action"
        );
        assert_eq!(app.world().resource::<SpaceMouseState>().buttons, 0);
    }

    #[test]
    fn hot_plug_through_the_fake_backend() {
        let fake = FakeSpaceMouse::default();
        let mut app = app(&fake);
        app.update();
        assert_eq!(app.world().resource::<SpaceMouseDevices>().connected(), 0);
        assert_eq!(app.world().resource::<SpaceMouseDevices>().backend, Some("fake"));

        fake.connect(7, "SpaceMouse Compact");
        fake.sample(
            7,
            SpaceMouseState {
                translation: [0.25, 0.0, 0.0],
                ..Default::default()
            },
        );
        fake.sample(
            7,
            SpaceMouseState {
                translation: [0.5, 0.0, 0.0],
                ..Default::default()
            },
        );
        app.update();
        assert_eq!(app.world().resource::<SpaceMouseDevices>().connected(), 1);
        assert_eq!(drain::<SpaceMouseConnected>(&mut app).len(), 1);
        assert_eq!(
            app.world().resource::<SpaceMouseState>().translation,
            [0.5, 0.0, 0.0],
            "latest sample, not the sum"
        );

        fake.connect(8, "SpaceMouse Pro");
        fake.sample(
            8,
            SpaceMouseState {
                translation: [0.75, 0.0, 0.0],
                ..Default::default()
            },
        );
        app.update();
        assert_eq!(
            app.world().resource::<SpaceMouseState>().translation[0],
            1.0,
            "devices merge, clamped"
        );

        fake.disconnect(7);
        fake.disconnect(8);
        app.update();
        assert_eq!(app.world().resource::<SpaceMouseDevices>().connected(), 0);
        assert_eq!(drain::<SpaceMouseDisconnected>(&mut app).len(), 2);
        assert!(
            app.world().resource::<SpaceMouseState>().is_idle(),
            "unplug zeroes the state"
        );

        fake.unavailable(
            "SpaceMouse Wireless",
            "permission denied opening /dev/hidraw3",
            true,
        );
        app.update();
        let devices = app.world().resource::<SpaceMouseDevices>();
        assert_eq!(devices.connected(), 0);
        assert!(matches!(
            devices.devices[0].status,
            SpaceMouseDeviceStatus::Unavailable(_)
        ));
        assert!(devices.warned_permission, "warned once");
    }

    #[test]
    fn buttons_fire_actions_and_toggles() {
        let fake = FakeSpaceMouse::default();
        let mut app = app(&fake);
        fake.connect(1, "SpaceMouse Enterprise");
        fake.sample(
            1,
            SpaceMouseState {
                buttons: 0b1101,
                ..Default::default()
            },
        );
        app.update();
        let actions: Vec<String> = drain::<SpaceMouseAction>(&mut app)
            .into_iter()
            .map(|a| a.action)
            .collect();
        assert_eq!(
            actions,
            [
                "view.fit_selection",
                "spacemouse.dominant_toggle",
                "spacemouse.horizon_lock_toggle"
            ]
        );
        let nav = *app.world().resource::<SpaceMouseNav>();
        assert!(nav.dominant && !nav.horizon_lock, "both toggles flipped: {nav:?}");
        fake.sample(1, SpaceMouseState::default());
        app.update();
        let released = drain::<SpaceMouseButton>(&mut app);
        assert_eq!(released.iter().filter(|b| !b.pressed).count(), 3, "{released:?}");
        assert!(
            drain::<SpaceMouseAction>(&mut app).is_empty(),
            "no actions on release"
        );
    }

    #[test]
    fn injection_replaces_device_state_for_its_frames() {
        let fake = FakeSpaceMouse::default();
        let mut app = app(&fake);
        fake.connect(1, "SpaceMouse");
        fake.sample(
            1,
            SpaceMouseState {
                rotation: [0.0, 0.3, 0.0],
                ..Default::default()
            },
        );
        let injected = SpaceMouseState {
            translation: [0.0, 0.0, -1.0],
            buttons: 0b10,
            ..Default::default()
        };
        app.world_mut()
            .resource_mut::<SpaceMouseInjection>()
            .inject(injected, 2);
        app.update();
        assert_eq!(*app.world().resource::<SpaceMouseState>(), injected);
        let actions = drain::<SpaceMouseAction>(&mut app);
        assert_eq!(actions[0].action, "view.home", "injected buttons fire actions");
        app.update();
        assert_eq!(
            *app.world().resource::<SpaceMouseState>(),
            injected,
            "second frame"
        );
        app.update();
        assert_eq!(
            app.world().resource::<SpaceMouseState>().rotation,
            [0.0, 0.3, 0.0],
            "back to the device"
        );
    }

    #[test]
    fn disabled_config_zeroes_the_state() {
        let fake = FakeSpaceMouse::default();
        let mut app = app(&fake);
        app.world_mut().resource_mut::<InputConfig>().spacemouse.enabled = false;
        fake.connect(1, "SpaceMouse");
        fake.sample(
            1,
            SpaceMouseState {
                translation: [1.0, 0.0, 0.0],
                ..Default::default()
            },
        );
        app.update();
        assert!(app.world().resource::<SpaceMouseState>().is_idle());
    }
}
