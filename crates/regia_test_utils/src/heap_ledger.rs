// SPDX-License-Identifier: MPL-2.0
//! The per-subsystem heap ledger in a test binary (ADR-2987).
//!
//! `regia_core::memory` counts heap bytes per engine subsystem only when its
//! [`TaggingAllocator`] is the process's global allocator. A test binary installs it with one
//! line at the top level of the test file, then reads the ledger:
//!
//! ```ignore
//! regia_test_utils::install_tagging_allocator!();
//!
//! #[test]
//! fn idle_holds_little() {
//!     let mut app = make_app();
//!     let idle = regia_test_utils::heap_ledger::idle_heap(120, || app.update());
//!     assert!(idle.growth_bytes() < 64 * 1024, "{}", idle.table());
//! }
//! ```
//!
//! [`idle_heap`] runs the frames it is given and returns what each subsystem holds afterwards and
//! what it gained, the same numbers `memory.idle_ledger` answers on the agent protocol.

pub use regia_core::memory::{
    MemorySnapshot, OwnerMemory, OwnerSnapshot, Subsystem, SubsystemMemory, TaggingAllocator,
};

/// Installs [`TaggingAllocator`] as the `#[global_allocator]` of the binary it is expanded in.
/// Expand it once, at the top level of a test file (a binary can have only one global allocator).
#[macro_export]
macro_rules! install_tagging_allocator {
    () => {
        #[global_allocator]
        static REGIA_TEST_TAGGING_ALLOCATOR: $crate::heap_ledger::TaggingAllocator =
            $crate::heap_ledger::TaggingAllocator::system();
    };
}

/// The heap ledger now, or `None` when [`install_tagging_allocator!`] was not expanded in this
/// binary (no allocation was ever counted).
#[must_use]
pub fn ledger() -> Option<MemorySnapshot> {
    let snapshot = regia_core::memory::snapshot();
    snapshot.tracking.then_some(snapshot)
}

/// The heap ledger now.
///
/// # Panics
/// When the tagging allocator is not installed: a test that reads heap bytes must install it.
#[must_use]
pub fn require_ledger() -> MemorySnapshot {
    match ledger() {
        Some(snapshot) => snapshot,
        None => panic!(
            "the tagging allocator is not installed: add `regia_test_utils::install_tagging_allocator!();` to the test file"
        ), // invariant: test helper; a misconfigured test binary must fail loudly
    }
}

/// What each subsystem holds after an idle window ([`idle_heap`]).
#[derive(Clone, Debug)]
pub struct IdleHeap {
    /// Frames the window ran.
    pub frames: u32,
    /// The ledger before the first frame.
    pub start: MemorySnapshot,
    /// The ledger after the last frame.
    pub end: MemorySnapshot,
    /// What stayed held through the window's tail ([`idle_heap_settled`]): each subsystem's
    /// smallest live bytes over the last frames, and the smallest total. Equal to `end` for
    /// [`idle_heap`]. A buffer kept while idle shows in every sample; a transient allocation on
    /// another thread (a worker mid-task when one sample was taken) does not.
    pub floor: MemorySnapshot,
    /// The largest total live bytes over the tail minus the smallest: the transient heap the
    /// window saw on top of what stayed held.
    pub transient_bytes: u64,
    /// The named owners ([`regia_core::memory::MemoryOwner`], ADR-3351) before the first frame.
    pub owners_start: OwnerSnapshot,
    /// The named owners after the last frame.
    pub owners_end: OwnerSnapshot,
    /// Each named owner's smallest live bytes over the tail, as [`Self::floor`] for subsystems.
    pub owners_floor: OwnerSnapshot,
}

impl IdleHeap {
    /// Heap bytes `subsystem` holds at the end.
    #[must_use]
    pub fn live_bytes(&self, subsystem: Subsystem) -> u64 {
        self.end.subsystem(subsystem).live_bytes
    }

    /// Change of `subsystem`'s live bytes over the window.
    #[must_use]
    pub fn growth(&self, subsystem: Subsystem) -> i64 {
        self.live_bytes(subsystem) as i64 - self.start.subsystem(subsystem).live_bytes as i64
    }

    /// Heap bytes `subsystem` held through the tail ([`Self::floor`]).
    #[must_use]
    pub fn floor_bytes(&self, subsystem: Subsystem) -> u64 {
        self.floor.subsystem(subsystem).live_bytes
    }

    /// Change of the total live bytes over the window.
    #[must_use]
    pub fn growth_bytes(&self) -> i64 {
        self.end.live_bytes as i64 - self.start.live_bytes as i64
    }

    /// Allocations `subsystem` made during the window (freed or not).
    #[must_use]
    pub fn allocations(&self, subsystem: Subsystem) -> u64 {
        self.end
            .subsystem(subsystem)
            .total_allocations
            .saturating_sub(self.start.subsystem(subsystem).total_allocations)
    }

    /// Allocations made during the window, all subsystems.
    #[must_use]
    pub fn total_allocations(&self) -> u64 {
        Subsystem::ALL.iter().map(|s| self.allocations(*s)).sum()
    }

    /// One row per subsystem: `(name, live bytes at the end, growth, allocations during)`, in
    /// [`Subsystem::ALL`] order.
    #[must_use]
    pub fn rows(&self) -> Vec<(&'static str, u64, i64, u64)> {
        Subsystem::ALL
            .iter()
            .map(|s| {
                (
                    s.name(),
                    self.live_bytes(*s),
                    self.growth(*s),
                    self.allocations(*s),
                )
            })
            .collect()
    }

    /// Heap bytes the owner called `name` held through the tail (0 when it never registered).
    #[must_use]
    pub fn owner_floor_bytes(&self, name: &str) -> u64 {
        self.owners_floor.live_bytes(name)
    }

    /// Change of the owner `name`'s live bytes over the window.
    #[must_use]
    pub fn owner_growth(&self, name: &str) -> i64 {
        self.owners_end.live_bytes(name) as i64 - self.owners_start.live_bytes(name) as i64
    }

    /// One row per named owner, largest held bytes first: `(name, subsystem, floor bytes, growth,
    /// allocations during)`.
    #[must_use]
    pub fn owner_rows(&self) -> Vec<(&'static str, Subsystem, u64, i64, u64)> {
        let mut rows: Vec<_> = self
            .owners_end
            .iter()
            .map(|o| {
                let before = self
                    .owners_start
                    .get(o.name)
                    .map_or(0, |b| b.memory.total_allocations);
                (
                    o.name,
                    o.subsystem,
                    self.owner_floor_bytes(o.name),
                    self.owner_growth(o.name),
                    o.memory.total_allocations.saturating_sub(before),
                )
            })
            .collect();
        rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(b.0)));
        rows
    }

    /// A fixed-width table of the subsystems that hold or gained anything, for assertion messages.
    #[must_use]
    pub fn table(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!(
            "idle heap after {} frames: {} B live, {:+} B, {} allocations\n",
            self.frames,
            self.end.live_bytes,
            self.growth_bytes(),
            self.total_allocations()
        );
        for (name, live, growth, allocations) in self.rows() {
            if live != 0 || growth != 0 || allocations != 0 {
                let _ = writeln!(
                    out,
                    "  {name:<11} {live:>12} B {growth:>+10} B {allocations:>8} allocs"
                );
            }
        }
        for (name, _, held, growth, allocations) in self.owner_rows() {
            if held != 0 || growth != 0 || allocations != 0 {
                let _ = writeln!(
                    out,
                    "    owner {name:<24} {held:>12} B {growth:>+10} B {allocations:>8} allocs"
                );
            }
        }
        out
    }
}

/// Runs `frames` frames through `step` (usually `|| app.update()`) and returns the ledger before
/// and after.
///
/// # Panics
/// When the tagging allocator is not installed ([`require_ledger`]).
pub fn idle_heap(frames: u32, step: impl FnMut()) -> IdleHeap {
    idle_heap_settled(frames, 1, step)
}

/// [`idle_heap`] that also samples the ledger after each of the last `tail` frames (at least 1,
/// at most `frames`) and keeps the per-subsystem minimum in [`IdleHeap::floor`].
///
/// # Panics
/// When the tagging allocator is not installed ([`require_ledger`]).
pub fn idle_heap_settled(frames: u32, tail: u32, step: impl FnMut()) -> IdleHeap {
    sample_idle_heap(frames, tail, step, require_ledger)
}

fn sample_idle_heap(
    frames: u32,
    tail: u32,
    mut step: impl FnMut(),
    mut snapshot: impl FnMut() -> MemorySnapshot,
) -> IdleHeap {
    let tail = tail.clamp(1, frames.max(1));
    let start = snapshot();
    let owners_start = regia_core::memory::owner_snapshot();
    let mut owners_floor: Option<OwnerSnapshot> = None;
    let mut floor: Option<MemorySnapshot> = None;
    // The last sample is the end: a second read after it would also count other threads'
    // allocations made in between (a test binary runs its tests in parallel).
    let mut last: Option<(MemorySnapshot, OwnerSnapshot)> = None;
    let mut highest = 0u64;
    for frame in 0..frames {
        step();
        if frame + tail < frames {
            continue;
        }
        let now = snapshot();
        let owners_now = regia_core::memory::owner_snapshot();
        last = Some((now, owners_now));
        owners_floor = Some(match owners_floor {
            None => owners_now,
            Some(mut low) => {
                low.floor_with(&owners_now);
                low
            }
        });
        highest = highest.max(now.live_bytes);
        floor = Some(match floor {
            None => now,
            Some(mut low) => {
                for (slot, (_, m)) in low.subsystems.iter_mut().zip(now.subsystems.iter()) {
                    slot.1.live_bytes = slot.1.live_bytes.min(m.live_bytes);
                    slot.1.live_allocations = slot.1.live_allocations.min(m.live_allocations);
                }
                low.live_bytes = low.live_bytes.min(now.live_bytes);
                low
            }
        });
    }
    let (end, owners_end) = last.unwrap_or_else(|| (snapshot(), regia_core::memory::owner_snapshot()));
    let floor = floor.unwrap_or(end);
    IdleHeap {
        frames,
        start,
        end,
        floor,
        transient_bytes: highest.saturating_sub(floor.live_bytes),
        owners_start,
        owners_end,
        owners_floor: owners_floor.unwrap_or(owners_end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // This crate's unit-test binary runs under the tagging allocator: the macro's own check.
    crate::install_tagging_allocator!();

    #[test]
    fn the_final_frame_sample_is_also_the_end_under_concurrent_changes() {
        let mut reads = 0;
        let idle = sample_idle_heap(
            4,
            1,
            || {},
            || {
                reads += 1;
                MemorySnapshot {
                    tracking: true,
                    live_bytes: reads,
                    peak_bytes: reads,
                    ..MemorySnapshot::default()
                }
            },
        );
        assert_eq!(idle.floor, idle.end, "do not read a later allocation as the end");
        assert_eq!(reads, 2, "one initial read and one final-frame read");
        assert_eq!(idle.end.live_bytes, 2);
        assert_eq!(idle.transient_bytes, 0);
    }

    #[test]
    fn zero_frames_and_long_tails_preserve_the_sampling_window() {
        for (frames, tail, expected_reads, expected_floor, expected_end) in
            [(0, 4, 2, 2, 2), (4, 2, 3, 2, 3), (2, 8, 3, 2, 3)]
        {
            let mut reads = 0;
            let mut steps = 0;
            let idle = sample_idle_heap(
                frames,
                tail,
                || steps += 1,
                || {
                    reads += 1;
                    MemorySnapshot {
                        tracking: true,
                        live_bytes: reads,
                        peak_bytes: reads,
                        ..MemorySnapshot::default()
                    }
                },
            );
            assert_eq!(steps, frames);
            assert_eq!(reads, expected_reads);
            assert_eq!(idle.floor.live_bytes, expected_floor);
            assert_eq!(idle.end.live_bytes, expected_end);
            assert_eq!(idle.transient_bytes, expected_end - expected_floor);
        }
    }

    #[test]
    fn the_macro_installs_the_ledger_and_idle_heap_sees_a_kept_block() {
        assert!(
            ledger().is_some(),
            "the allocator counts from the first allocation"
        );
        let mut kept: Vec<Vec<u8>> = Vec::new();
        let idle = idle_heap(4, || {
            let _scope = regia_core::memory::enter(Subsystem::Gameplay);
            kept.push(vec![0u8; 4096]);
        });
        assert_eq!(idle.frames, 4);
        // Other tests of this binary allocate gameplay bytes on parallel threads only if they
        // enter the scope; none does, so the four kept blocks are the growth.
        assert!(idle.growth(Subsystem::Gameplay) >= 4 * 4096, "{}", idle.table());
        assert!(idle.allocations(Subsystem::Gameplay) >= 4);
        assert!(idle.table().contains("gameplay"));
        let rows = idle.rows();
        assert_eq!(rows.len(), Subsystem::COUNT);
        assert_eq!(idle.floor, idle.end, "a one-frame tail is the end");
        drop(kept);
    }

    #[test]
    fn the_settled_floor_drops_a_transient_and_keeps_a_held_block() {
        // Eight frames: a block held from frame 2 on, and on frame 6 only a 1 MiB block that is
        // still alive when that frame's sample is taken.
        let mut held: Option<Vec<u8>> = None;
        let mut transient: Option<Vec<u8>> = None;
        let mut frame = 0;
        let idle = idle_heap_settled(8, 4, || {
            let _scope = regia_core::memory::enter(Subsystem::Telemetry);
            transient = None;
            if frame == 2 {
                held = Some(vec![1u8; 8192]);
            }
            if frame == 6 {
                transient = Some(vec![2u8; 1 << 20]);
            }
            frame += 1;
        });
        let floor = idle.floor_bytes(Subsystem::Telemetry);
        let start = idle.start.subsystem(Subsystem::Telemetry).live_bytes;
        // No other test of this binary enters the telemetry scope: the floor holds the kept block
        // and not the transient one.
        assert!(floor >= start + 8192, "{}", idle.table());
        assert!(floor < start + (1 << 20), "{}", idle.table());
        drop((held, transient));
    }

    #[test]
    fn a_named_owner_gets_its_own_floor_and_row() {
        static OWNER: regia_core::memory::MemoryOwner =
            regia_core::memory::MemoryOwner::new(Subsystem::Vfx, "vfx.heap_ledger_test");
        let mut held: Vec<Vec<u8>> = Vec::new();
        let idle = idle_heap_settled(4, 2, || {
            let _owner = regia_core::memory::enter_owner(&OWNER);
            held.push(vec![3u8; 2048]);
        });
        assert!(
            idle.owner_floor_bytes("vfx.heap_ledger_test") >= 3 * 2048,
            "{}",
            idle.table()
        );
        assert!(idle.owner_growth("vfx.heap_ledger_test") >= 4 * 2048);
        let row = idle
            .owner_rows()
            .into_iter()
            .find(|r| r.0 == "vfx.heap_ledger_test")
            .expect("the owner has a row");
        assert_eq!(row.1, Subsystem::Vfx);
        assert!(row.4 >= 4);
        assert!(idle.table().contains("owner vfx.heap_ledger_test"));
        // The owner's bytes are also the subsystem's.
        assert!(idle.growth(Subsystem::Vfx) >= 4 * 2048);
        drop(held);
    }
}
