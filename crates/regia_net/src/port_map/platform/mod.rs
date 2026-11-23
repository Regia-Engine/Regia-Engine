// SPDX-License-Identifier: MPL-2.0
//! The default IPv4 gateway per operating system (ADR-8893; `docs/04-conventions.md` § 8). Each
//! backend exposes `default_gateway_v4`; the byte layouts are parsed in `port_map::route_table`.

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) use linux::default_gateway_v4;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(super) use windows::default_gateway_v4;

#[cfg(target_vendor = "apple")]
mod apple;
#[cfg(target_vendor = "apple")]
pub(super) use apple::default_gateway_v4;

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple", windows)))]
mod other;
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple", windows)))]
pub(super) use other::default_gateway_v4;
