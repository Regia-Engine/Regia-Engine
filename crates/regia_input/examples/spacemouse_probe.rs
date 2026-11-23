// SPDX-License-Identifier: MPL-2.0
//! Hardware check for the SpaceMouse backend (`docs/specs/29-spacemouse.md`): opens every
//! SpaceMouse through the real HID backend and prints the devices, the merged normalised state
//! and button actions for a few seconds. Use it to confirm the axis signs on a new model:
//! push right → +x, lift → +y, pull toward you → +z, tilt away → +rx, twist counter-clockwise
//! (seen from above) → +ry, tilt left → +rz.
//!
//! ```text
//! cargo run -p regia_input --example spacemouse_probe -- [seconds]
//! ```
//! On Linux it reads the spacenavd socket when the daemon runs (the device shows up on the first
//! motion), otherwise the hidraw node, which needs the udev rule from
//! `book/user-guide/spacemouse.md`. The device name shows which source is active. Desktop only.

#[cfg(target_os = "android")]
fn main() {}

#[cfg(not(target_os = "android"))]
fn main() {
    use regia_app::prelude::*;
    use regia_input::InputPlugin;
    use regia_input::spacemouse::{SpaceMouseAction, SpaceMouseDevices, SpaceMouseState};

    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(15);
    let mut app = App::new();
    app.add_plugins(InputPlugin::default());
    let start = std::time::Instant::now();
    let mut last = (usize::MAX, SpaceMouseState::default());
    while start.elapsed().as_secs() < seconds {
        app.update();
        let world = app.world_mut();
        let devices = world.resource::<SpaceMouseDevices>().clone();
        let state = *world.resource::<SpaceMouseState>();
        if (devices.devices.len(), state) != last {
            last = (devices.devices.len(), state);
            println!(
                "devices {:?} | t [{:+.2} {:+.2} {:+.2}] r [{:+.2} {:+.2} {:+.2}] buttons {:#06b}",
                devices
                    .devices
                    .iter()
                    .map(|d| format!("{} [{}] ({:?})", d.info.name, d.info.source, d.status))
                    .collect::<Vec<_>>(),
                state.translation[0],
                state.translation[1],
                state.translation[2],
                state.rotation[0],
                state.rotation[1],
                state.rotation[2],
                state.buttons,
            );
        }
        for action in world.resource_mut::<Messages<SpaceMouseAction>>().drain() {
            println!("action {} (button {})", action.action, action.button);
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}
