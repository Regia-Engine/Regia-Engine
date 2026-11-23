// SPDX-License-Identifier: MPL-2.0
//! The residency keeper's OS seam (`docs/04-conventions.md` § 8).

#[cfg(any(target_os = "linux", target_os = "android"))]
mod procfs;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) use procfs::Sweeper;

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod unsupported;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(super) use unsupported::Sweeper;
