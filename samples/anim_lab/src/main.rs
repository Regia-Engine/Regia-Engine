// SPDX-License-Identifier: MIT-0
//! Headless animation lab.
//!
//! Plays the Khronos Fox and CesiumMan clips from a gitignored cache and exercises the
//! M12 graph: idle/walk/run blend, start/loop/land, an upper-body wave, an additive lean,
//! two-bone IK, look-at, footstep notifies, and sockets (weapon, pilot, turret), and a montage
//! played through an animation graph `Slot` node (forward, reversed and time-stretched, and as a
//! child montage; `slot_graph`), and IK, look-at and turret aim on targets that move every frame
//! (`anim_lab::reach`, the library part its tests run). The renderer preview is the render lane's milestone; this
//! sample checks the CPU pose, skin palette and audio notifies.

use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(target_os = "android")]
    {
        eprintln!("anim_lab imports glTF on the desktop host");
        ExitCode::SUCCESS
    }
    #[cfg(not(target_os = "android"))]
    lab::run()
}

#[cfg(not(target_os = "android"))]
mod lab;
#[cfg(not(target_os = "android"))]
mod slot_graph;
