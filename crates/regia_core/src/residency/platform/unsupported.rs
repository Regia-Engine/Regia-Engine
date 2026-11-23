// SPDX-License-Identifier: MPL-2.0
//! Platforms without a residency keeper yet (Windows, macOS): [`Sweeper::new`] is `None`.
//!
//! Windows could use `QueryWorkingSetEx` + reads of the image's sections, or
//! `PrefetchVirtualMemory`; not done until a Windows soak shows the stall.

use std::sync::atomic::AtomicBool;

use super::super::Sweep;

/// Never constructed.
pub(in crate::residency) struct Sweeper;

impl Sweeper {
    pub(in crate::residency) fn new() -> Option<Self> {
        None
    }

    pub(in crate::residency) fn sweep(&mut self, _code: bool, _stop: &AtomicBool) -> Option<Sweep> {
        None
    }
}
