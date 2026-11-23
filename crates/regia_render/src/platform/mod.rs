// SPDX-License-Identifier: MPL-2.0
//! Platform seams of the renderer (`docs/04-conventions.md` § 8).

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
mod other;
#[cfg(windows)]
mod windows;

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) use linux::{
    at_exit, device_allocated_bytes, keeps_vulkan_drivers_loaded, lower_current_thread_priority, process_age,
};
#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
pub(crate) use linux::{at_exit_with_status, exit_now};
#[cfg(target_os = "macos")]
pub(crate) use macos::{
    at_exit, device_allocated_bytes, keeps_vulkan_drivers_loaded, lower_current_thread_priority, process_age,
};
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
pub(crate) use other::{
    at_exit, device_allocated_bytes, keeps_vulkan_drivers_loaded, lower_current_thread_priority, process_age,
};
#[cfg(windows)]
pub(crate) use windows::{
    at_exit, device_allocated_bytes, keeps_vulkan_drivers_loaded, lower_current_thread_priority, process_age,
};
