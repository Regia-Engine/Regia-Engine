// SPDX-License-Identifier: MPL-2.0
//! Allocation counters for the scale numbers (N215, ADR-8454).
//!
//! The crate has no allocator of its own: a test binary with a counting global allocator hands its
//! thread-local allocation count to [`install`], and every run then records the allocations the
//! server makes in each ticking iteration ([`super::workload::ServerRecord::tick_allocs`]). The
//! impairment network's own copies (one `Vec` per datagram a socket sends, which a real socket
//! would not make) are measured around each [`super::impair::ImpairedSocket`] call and subtracted,
//! so the number is the engine's and the sample game's.

use std::cell::Cell;
use std::sync::OnceLock;

/// The calling thread's allocation count so far (allocations, not bytes).
pub type AllocCount = fn() -> u64;

static PROBE: OnceLock<AllocCount> = OnceLock::new();

thread_local! {
    static HARNESS: Cell<u64> = const { Cell::new(0) };
}

/// Installs the allocation counter. Returns `false` when one was already installed (the first
/// stays).
pub fn install(count: AllocCount) -> bool {
    PROBE.set(count).is_ok()
}

/// Whether a counter is installed.
pub fn installed() -> bool {
    PROBE.get().is_some()
}

/// The calling thread's allocation count, `None` without a counter.
pub fn allocs() -> Option<u64> {
    PROBE.get().map(|f| f())
}

/// Runs `f` and books its allocations to the harness (subtracted from the engine's numbers).
pub fn harness<R>(f: impl FnOnce() -> R) -> R {
    let Some(count) = PROBE.get() else { return f() };
    let before = count();
    let out = f();
    let used = count().saturating_sub(before);
    HARNESS.with(|h| h.set(h.get() + used));
    out
}

/// Allocations booked to the harness on this thread so far.
pub fn harness_allocs() -> u64 {
    HARNESS.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_is_free_without_a_counter() {
        // No counter in this binary's unit tests: nothing is booked and the closure still runs.
        if installed() {
            return;
        }
        assert_eq!(harness(|| 7), 7);
        assert_eq!(harness_allocs(), 0);
        assert_eq!(allocs(), None);
    }
}
