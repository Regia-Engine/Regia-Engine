// SPDX-License-Identifier: MPL-2.0
//! Shutdown and reload requests (spec 76 § 5).
//!
//! The OS handlers only set atomic flags; the host reads them at the start of a tick. Unix:
//! SIGTERM and SIGINT request a shutdown (the second one exits at once), SIGHUP a reload,
//! registered with `signal-hook`'s safe flag API (ADR-1663). Windows: Ctrl+C, Ctrl+Break and
//! console close request a shutdown through `regia_platform::console_ctrl`, a second event while
//! draining exits at once, and reload is the console command `reload` (ADR-1886).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The host's request flags. Cloning shares them.
#[derive(Debug, Clone, Default)]
pub struct HostSignals {
    pub(super) shutdown: Arc<AtomicBool>,
    pub(super) reload: Arc<AtomicBool>,
    pub(super) draining: Arc<AtomicBool>,
    pub(super) stopped: Arc<AtomicBool>,
    /// `server.rotation_skip`: the host travels at the start of the next tick.
    pub(super) rotation_skip: Arc<AtomicBool>,
}

impl HostSignals {
    /// Fresh flags.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests a shutdown: the same path the signal handler takes.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Requests a reload (SIGHUP's path).
    pub fn request_reload(&self) {
        self.reload.store(true, Ordering::SeqCst);
    }

    /// Marks the host as draining: from now on a SIGTERM or SIGINT exits the process at once
    /// (code 0), even while a tick is stuck.
    pub fn mark_draining(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    /// True once [`Self::mark_draining`] ran.
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    /// Marks the host as stopped with its log flushed: a console close handler stops waiting.
    pub fn mark_stopped(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    /// True once [`Self::mark_stopped`] ran.
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Takes a pending shutdown request.
    pub fn take_shutdown(&self) -> bool {
        self.shutdown.swap(false, Ordering::SeqCst)
    }

    /// Takes a pending reload request.
    pub fn take_reload(&self) -> bool {
        self.reload.swap(false, Ordering::SeqCst)
    }

    /// Requests a rotation skip (`server.rotation_skip`). The host travels on its next tick.
    pub fn request_rotation_skip(&self) {
        self.rotation_skip.store(true, Ordering::SeqCst);
    }

    /// Takes a pending rotation skip.
    pub fn take_rotation_skip(&self) -> bool {
        self.rotation_skip.swap(false, Ordering::SeqCst)
    }

    /// Registers the OS handlers and returns what was registered. Unix: SIGTERM, SIGINT
    /// (shutdown) and SIGHUP (reload). Windows: Ctrl+C, Ctrl+Break and console close (shutdown);
    /// reload is the `reload` console command there. Other targets: none (use the console
    /// commands `quit` and `reload`). The per-OS code is in `server_host/platform/`.
    pub fn install_os_handlers(&self) -> std::io::Result<Vec<&'static str>> {
        super::platform::install_os_handlers(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_taken_once() {
        let s = HostSignals::new();
        let shared = s.clone();
        shared.request_shutdown();
        assert!(s.take_shutdown());
        assert!(!s.take_shutdown());
        s.request_reload();
        assert!(shared.take_reload());
    }
}
