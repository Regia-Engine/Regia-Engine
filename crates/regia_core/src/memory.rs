// SPDX-License-Identifier: MPL-2.0
//! CPU memory accounting by engine subsystem (`docs/specs/10-core.md` § 3.3, ADR-539).
//!
//! - [`TaggingAllocator`] is an opt-in global allocator. A host installs it in dev builds,
//!   tests and soaks (`#[global_allocator] static A: TaggingAllocator = TaggingAllocator::system();`);
//!   every heap allocation is then counted against the [`Subsystem`] the allocating thread was
//!   working for, and freed against the same one, wherever the free happens.
//! - [`enter`] marks the running thread as working for a subsystem until the returned
//!   [`MemoryScope`] drops. Scopes nest: the innermost wins. Work outside every scope counts as
//!   [`Subsystem::Unscoped`]. Engine entry points (level spawn and unload, asset loads, the
//!   physics step, the audio drain, the frame render) open their own scope.
//! - [`snapshot`] reads the ledger; [`MemorySnapshot::growth_since`] compares two snapshots and
//!   [`LeakCheck`] judges a series of samples taken over repeated load/unload cycles.
//!
//! Without the allocator installed nothing is counted: [`tracking_installed`] is `false` and every
//! total is 0, and [`enter`] costs one thread-local store. With it, each allocation costs a
//! 16-byte (or alignment-sized) header and a handful of relaxed atomic adds.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

mod owners;
mod tagging;

pub use owners::{
    MAX_OWNERS, MemoryOwner, OwnerMemory, OwnerSnapshot, current_owner, enter_owner, owner_snapshot,
    scoped_owner,
};
pub use tagging::TaggingAllocator;

/// What part of the engine a heap allocation was made for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Subsystem {
    /// Outside every scope: host code, start-up, the standard library, worker pools.
    #[default]
    Unscoped = 0,
    /// Scene and level spawning, unloading and snapshots (`regia_scene`).
    Scene,
    /// Asset loads and decoding (`regia_asset`).
    Assets,
    /// The renderer's CPU side: frame building, uploads, caches (`regia_render`).
    Render,
    /// Terrain streaming, scatter and simulation (`regia_terrain`).
    Terrain,
    /// The physics step (`regia_physics`).
    Physics,
    /// Audio voices and mixing requests (`regia_audio`).
    Audio,
    /// Animation evaluation (`regia_animation`).
    Animation,
    /// Navigation meshes and queries (`regia_nav`).
    Navigation,
    /// Runtime UI (`regia_ui`).
    Ui,
    /// Particles and decals (`regia_vfx`).
    Vfx,
    /// Telemetry records and sinks (`regia_telemetry`).
    Telemetry,
    /// The agent protocol server (`regia_agent`).
    Agent,
    /// Game code that opens its own scope.
    Gameplay,
}

impl Subsystem {
    /// Number of subsystems.
    pub const COUNT: usize = 14;

    /// Every subsystem, in report order (the discriminant order).
    pub const ALL: [Self; Self::COUNT] = [
        Self::Unscoped,
        Self::Scene,
        Self::Assets,
        Self::Render,
        Self::Terrain,
        Self::Physics,
        Self::Audio,
        Self::Animation,
        Self::Navigation,
        Self::Ui,
        Self::Vfx,
        Self::Telemetry,
        Self::Agent,
        Self::Gameplay,
    ];

    /// Stable snake-case name (agent protocol, telemetry).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Unscoped => "unscoped",
            Self::Scene => "scene",
            Self::Assets => "assets",
            Self::Render => "render",
            Self::Terrain => "terrain",
            Self::Physics => "physics",
            Self::Audio => "audio",
            Self::Animation => "animation",
            Self::Navigation => "navigation",
            Self::Ui => "ui",
            Self::Vfx => "vfx",
            Self::Telemetry => "telemetry",
            Self::Agent => "agent",
            Self::Gameplay => "gameplay",
        }
    }

    /// The subsystem named `name` ([`Subsystem::name`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The subsystem stored in an allocation header; an unknown tag is [`Subsystem::Unscoped`].
    fn from_tag(tag: u8) -> Self {
        Self::ALL.get(usize::from(tag)).copied().unwrap_or(Self::Unscoped)
    }
}

impl std::fmt::Display for Subsystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// One subsystem's counters.
struct Slot {
    live_bytes: AtomicU64,
    peak_bytes: AtomicU64,
    live_allocations: AtomicU64,
    total_allocations: AtomicU64,
    total_bytes: AtomicU64,
}

impl Slot {
    const fn new() -> Self {
        Self {
            live_bytes: AtomicU64::new(0),
            peak_bytes: AtomicU64::new(0),
            live_allocations: AtomicU64::new(0),
            total_allocations: AtomicU64::new(0),
            total_bytes: AtomicU64::new(0),
        }
    }
}

static LEDGER: [Slot; Subsystem::COUNT] = [const { Slot::new() }; Subsystem::COUNT];
static LIVE_TOTAL: AtomicU64 = AtomicU64::new(0);
static PEAK_TOTAL: AtomicU64 = AtomicU64::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The running thread's subsystem, as its `u8` tag. Const-initialized with no destructor,
    /// so the allocator may read it at any time (including during thread teardown).
    static CURRENT: Cell<u8> = const { Cell::new(0) };
    /// The running thread's [`MemoryOwner`] row (0: none), set with [`enter_owner`]. Same rules as
    /// `CURRENT`.
    static CURRENT_OWNER: Cell<u8> = const { Cell::new(0) };
}

fn slot(tag: u8) -> &'static Slot {
    LEDGER.get(usize::from(tag)).unwrap_or(&LEDGER[0])
}

/// The running thread's tag (0 when its thread-local is already gone).
fn current_tag() -> u8 {
    CURRENT.try_with(Cell::get).unwrap_or(0)
}

/// The running thread's owner row (0 when none, or when its thread-local is already gone).
fn current_owner_tag() -> u8 {
    CURRENT_OWNER.try_with(Cell::get).unwrap_or(0)
}

/// Counts a new allocation of `bytes` against `tag` and owner row `owner` (0: none).
fn record_alloc(tag: u8, owner: u8, bytes: usize) {
    if !INSTALLED.load(Ordering::Relaxed) {
        INSTALLED.store(true, Ordering::Relaxed);
    }
    let bytes = bytes as u64;
    let s = slot(tag);
    let live = s.live_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
    s.peak_bytes.fetch_max(live, Ordering::Relaxed);
    s.live_allocations.fetch_add(1, Ordering::Relaxed);
    s.total_allocations.fetch_add(1, Ordering::Relaxed);
    s.total_bytes.fetch_add(bytes, Ordering::Relaxed);
    let total = LIVE_TOTAL.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK_TOTAL.fetch_max(total, Ordering::Relaxed);
    if let Some(o) = owners::owner_slot(owner) {
        let live = o.live_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        o.peak_bytes.fetch_max(live, Ordering::Relaxed);
        o.live_allocations.fetch_add(1, Ordering::Relaxed);
        o.total_allocations.fetch_add(1, Ordering::Relaxed);
        o.total_bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

/// Counts the free of an allocation of `bytes` made against `tag` and owner row `owner`.
fn record_free(tag: u8, owner: u8, bytes: usize) {
    let bytes = bytes as u64;
    let s = slot(tag);
    s.live_bytes.fetch_sub(bytes, Ordering::Relaxed);
    s.live_allocations.fetch_sub(1, Ordering::Relaxed);
    LIVE_TOTAL.fetch_sub(bytes, Ordering::Relaxed);
    if let Some(o) = owners::owner_slot(owner) {
        o.live_bytes.fetch_sub(bytes, Ordering::Relaxed);
        o.live_allocations.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Counts an allocation of `tag` resized in place or moved from `old` to `new` bytes. It stays
/// with the subsystem that made it.
fn record_resize(tag: u8, owner: u8, old: usize, new: usize) {
    let s = slot(tag);
    let o = owners::owner_slot(owner);
    if new >= old {
        let grown = (new - old) as u64;
        let live = s.live_bytes.fetch_add(grown, Ordering::Relaxed) + grown;
        s.peak_bytes.fetch_max(live, Ordering::Relaxed);
        s.total_bytes.fetch_add(grown, Ordering::Relaxed);
        let total = LIVE_TOTAL.fetch_add(grown, Ordering::Relaxed) + grown;
        PEAK_TOTAL.fetch_max(total, Ordering::Relaxed);
        if let Some(o) = o {
            let live = o.live_bytes.fetch_add(grown, Ordering::Relaxed) + grown;
            o.peak_bytes.fetch_max(live, Ordering::Relaxed);
            o.total_bytes.fetch_add(grown, Ordering::Relaxed);
        }
    } else {
        let shrunk = (old - new) as u64;
        s.live_bytes.fetch_sub(shrunk, Ordering::Relaxed);
        LIVE_TOTAL.fetch_sub(shrunk, Ordering::Relaxed);
        if let Some(o) = o {
            o.live_bytes.fetch_sub(shrunk, Ordering::Relaxed);
        }
    }
}

/// While alive, heap allocations on this thread count against one [`Subsystem`] ([`enter`]).
/// Dropping it restores the subsystem that was current before. Not `Send`: a scope belongs to
/// the thread that opened it.
#[must_use = "the scope ends when the guard drops"]
pub struct MemoryScope {
    previous: u8,
    previous_owner: u8,
    _thread_bound: PhantomData<*const ()>,
}

impl Drop for MemoryScope {
    fn drop(&mut self) {
        let previous = self.previous;
        let _ = CURRENT.try_with(|c| c.set(previous));
        let previous_owner = self.previous_owner;
        let _ = CURRENT_OWNER.try_with(|c| c.set(previous_owner));
    }
}

/// Counts this thread's heap allocations against `subsystem` until the guard drops.
///
/// `let _mem = regia_core::memory::enter(Subsystem::Assets);`
///
/// Inside an owner scope ([`enter_owner`]) of another subsystem this clears the owner until the
/// guard drops; inside one of the same subsystem the owner stays (a renderer function that opens
/// its own `Render` scope is still the enclosing render owner's).
pub fn enter(subsystem: Subsystem) -> MemoryScope {
    let owner = current_owner_tag();
    let keep = owner != 0 && owners::owner_subsystem(owner) == Some(subsystem);
    enter_tagged(subsystem, if keep { owner } else { 0 })
}

/// Sets the thread's subsystem and owner row; the guard restores both.
fn enter_tagged(subsystem: Subsystem, owner: u8) -> MemoryScope {
    let previous = CURRENT.try_with(|c| c.replace(subsystem as u8)).unwrap_or(0);
    let previous_owner = CURRENT_OWNER.try_with(|c| c.replace(owner)).unwrap_or(0);
    MemoryScope {
        previous,
        previous_owner,
        _thread_bound: PhantomData,
    }
}

/// Runs `f` inside a [`Subsystem`] scope ([`enter`]).
pub fn scoped<R>(subsystem: Subsystem, f: impl FnOnce() -> R) -> R {
    let _scope = enter(subsystem);
    f()
}

/// The subsystem this thread is working for.
#[must_use]
pub fn current() -> Subsystem {
    Subsystem::from_tag(current_tag())
}

/// Whether a [`TaggingAllocator`] is the process's global allocator (it has counted at least
/// one allocation).
#[must_use]
pub fn tracking_installed() -> bool {
    INSTALLED.load(Ordering::Relaxed)
}

/// Live heap bytes across every subsystem, when a [`TaggingAllocator`] is installed.
#[must_use]
pub fn live_heap_bytes() -> Option<u64> {
    tracking_installed().then(|| LIVE_TOTAL.load(Ordering::Relaxed))
}

/// One subsystem's heap use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubsystemMemory {
    /// Bytes allocated and not yet freed.
    pub live_bytes: u64,
    /// Highest `live_bytes` seen.
    pub peak_bytes: u64,
    /// Allocations not yet freed.
    pub live_allocations: u64,
    /// Allocations made since start-up.
    pub total_allocations: u64,
    /// Bytes allocated since start-up (growth by reallocation included).
    pub total_bytes: u64,
}

/// The heap ledger at (nearly) one moment ([`snapshot`]). The default is an untracked, all-zero ledger
/// that still lists every subsystem in [`Subsystem::ALL`] order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemorySnapshot {
    /// Whether a [`TaggingAllocator`] is installed; `false` means every number is 0.
    pub tracking: bool,
    /// Live heap bytes, all subsystems.
    pub live_bytes: u64,
    /// Highest `live_bytes` seen.
    pub peak_bytes: u64,
    /// Per subsystem, in [`Subsystem::ALL`] order.
    pub subsystems: [(Subsystem, SubsystemMemory); Subsystem::COUNT],
}

impl Default for MemorySnapshot {
    fn default() -> Self {
        Self {
            tracking: false,
            live_bytes: 0,
            peak_bytes: 0,
            subsystems: Subsystem::ALL.map(|subsystem| (subsystem, SubsystemMemory::default())),
        }
    }
}

/// Reads the heap ledger. Allocation-free; a few dozen relaxed atomic loads.
///
/// The loads are not one atomic read: an allocation on another thread while they run can be in
/// some counters and not yet in others. The snapshot is made self-consistent instead: the total
/// live bytes are the sum of the subsystem live bytes read in the same pass, and no peak is
/// below its live value (ADR-2396).
#[must_use]
pub fn snapshot() -> MemorySnapshot {
    let subsystems: [(Subsystem, SubsystemMemory); Subsystem::COUNT] = std::array::from_fn(|i| {
        let s = &LEDGER[i];
        let live_bytes = s.live_bytes.load(Ordering::Relaxed);
        (
            Subsystem::ALL[i],
            SubsystemMemory {
                live_bytes,
                peak_bytes: s.peak_bytes.load(Ordering::Relaxed).max(live_bytes),
                live_allocations: s.live_allocations.load(Ordering::Relaxed),
                total_allocations: s.total_allocations.load(Ordering::Relaxed),
                total_bytes: s.total_bytes.load(Ordering::Relaxed),
            },
        )
    });
    let live_bytes = subsystems
        .iter()
        .fold(0u64, |sum, (_, memory)| sum.saturating_add(memory.live_bytes));
    MemorySnapshot {
        tracking: tracking_installed(),
        live_bytes,
        peak_bytes: PEAK_TOTAL.load(Ordering::Relaxed).max(live_bytes),
        subsystems,
    }
}

impl MemorySnapshot {
    /// `subsystem`'s use.
    #[must_use]
    pub fn subsystem(&self, subsystem: Subsystem) -> SubsystemMemory {
        self.subsystems[subsystem as usize].1
    }

    /// Live-byte and live-allocation change of every subsystem from `earlier` to `self`.
    #[must_use]
    pub fn growth_since(&self, earlier: &Self) -> MemoryGrowth {
        let delta = |now: u64, then: u64| now as i64 - then as i64;
        MemoryGrowth {
            live_bytes: delta(self.live_bytes, earlier.live_bytes),
            subsystems: std::array::from_fn(|i| {
                let (subsystem, now) = self.subsystems[i];
                let then = earlier.subsystems[i].1;
                (
                    subsystem,
                    delta(now.live_bytes, then.live_bytes),
                    delta(now.live_allocations, then.live_allocations),
                )
            }),
        }
    }

    /// A fixed-width table for logs: totals, then one row per subsystem that ever allocated.
    #[must_use]
    pub fn table(&self) -> String {
        use std::fmt::Write as _;
        let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
        let mut out = String::new();
        if !self.tracking {
            out.push_str("CPU heap: not tracked (no TaggingAllocator installed)\n");
            return out;
        }
        let _ = writeln!(
            out,
            "CPU heap: {:.1} MiB live, {:.1} MiB peak",
            mib(self.live_bytes),
            mib(self.peak_bytes)
        );
        for (subsystem, m) in &self.subsystems {
            if m.total_allocations > 0 {
                let _ = writeln!(
                    out,
                    "  {:<11} {:>9.2} MiB live {:>9.2} MiB peak {:>8} allocs",
                    subsystem.name(),
                    mib(m.live_bytes),
                    mib(m.peak_bytes),
                    m.live_allocations
                );
            }
        }
        out
    }
}

/// The change between two [`MemorySnapshot`]s ([`MemorySnapshot::growth_since`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryGrowth {
    /// Live heap bytes gained (negative: freed).
    pub live_bytes: i64,
    /// `(subsystem, live bytes gained, live allocations gained)` in [`Subsystem::ALL`] order.
    pub subsystems: [(Subsystem, i64, i64); Subsystem::COUNT],
}

impl MemoryGrowth {
    /// Live bytes `subsystem` gained.
    #[must_use]
    pub fn bytes(&self, subsystem: Subsystem) -> i64 {
        self.subsystems[subsystem as usize].1
    }

    /// The subsystem that gained the most live bytes, with the amount (`None` when none grew).
    #[must_use]
    pub fn largest(&self) -> Option<(Subsystem, i64)> {
        self.subsystems
            .iter()
            .filter(|(_, bytes, _)| *bytes > 0)
            .max_by_key(|(_, bytes, _)| *bytes)
            .map(|(subsystem, bytes, _)| (*subsystem, *bytes))
    }
}

/// Judges whether a value stays bounded over many repeated cycles (load a level, unload it, …).
///
/// Feed it one sample per cycle ([`LeakCheck::sample`]). The first `warmup` cycles are ignored:
/// caches, pools and container capacities settle there. The rest is split into windows of
/// `window` cycles, and the growth is the peak of the last window minus the peak of the first.
/// Peaks, not single samples, so a buffer that happens to be full at one sample and empty at
/// the next does not read as growth. A leak of `b` bytes per cycle shows up as about
/// `b × (cycles − warmup − window)` bytes of growth.
#[derive(Clone, Debug)]
pub struct LeakCheck {
    warmup: u64,
    window: u64,
    cycles: u64,
    first_peak: Option<u64>,
    first_window_end: u64,
    current_peak: u64,
    current_window_start: u64,
    last_peak: Option<u64>,
}

/// What a [`LeakCheck`] measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LeakVerdict {
    /// Cycles sampled (warm-up included).
    pub cycles: u64,
    /// Peak of the first window after warm-up.
    pub first_peak: u64,
    /// Peak of the last complete window.
    pub last_peak: u64,
    /// `last_peak − first_peak` (negative: shrank).
    pub growth: i64,
    /// The bound it was judged against.
    pub bound: u64,
    /// `true` when `growth ≤ bound`.
    pub bounded: bool,
}

impl LeakCheck {
    /// A check that skips `warmup` cycles and takes peaks over windows of `window` cycles
    /// (`window` is at least 1).
    #[must_use]
    pub fn new(warmup: u64, window: u64) -> Self {
        Self {
            warmup,
            window: window.max(1),
            cycles: 0,
            first_peak: None,
            first_window_end: warmup.saturating_add(window.max(1)),
            current_peak: 0,
            current_window_start: warmup,
            last_peak: None,
        }
    }

    /// Records the value after one more cycle.
    pub fn sample(&mut self, value: u64) {
        let cycle = self.cycles;
        self.cycles += 1;
        if cycle < self.warmup {
            return;
        }
        if cycle == self.current_window_start {
            self.current_peak = value;
        } else {
            self.current_peak = self.current_peak.max(value);
        }
        if self.cycles == self.current_window_start.saturating_add(self.window) {
            if self.cycles == self.first_window_end {
                self.first_peak = Some(self.current_peak);
            }
            self.last_peak = Some(self.current_peak);
            self.current_window_start = self.cycles;
        }
    }

    /// Cycles sampled so far.
    #[must_use]
    pub fn cycles(&self) -> u64 {
        self.cycles
    }

    /// The growth so far, judged against `bound` bytes. `None` until two windows after warm-up
    /// are complete.
    #[must_use]
    pub fn verdict(&self, bound: u64) -> Option<LeakVerdict> {
        let first_peak = self.first_peak?;
        let last_peak = self.last_peak?;
        if self.cycles < self.first_window_end.saturating_add(self.window) {
            return None;
        }
        // Wide arithmetic: peaks past `i64::MAX` must not overflow, nor a bound past it wrap negative.
        let growth = (i128::from(last_peak) - i128::from(first_peak))
            .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
        Some(LeakVerdict {
            cycles: self.cycles,
            first_peak,
            last_peak,
            growth,
            bound,
            bounded: i128::from(growth) <= i128::from(bound),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_tags_are_discriminants() {
        for (i, s) in Subsystem::ALL.into_iter().enumerate() {
            assert_eq!(s as usize, i);
            assert_eq!(Subsystem::from_name(s.name()), Some(s));
            assert_eq!(Subsystem::from_tag(i as u8), s);
            assert!(s.name().bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        }
        assert_eq!(Subsystem::from_tag(200), Subsystem::Unscoped);
        assert_eq!(Subsystem::from_name("nope"), None);
    }

    #[test]
    fn scopes_nest_and_restore() {
        assert_eq!(current(), Subsystem::Unscoped);
        {
            let _a = enter(Subsystem::Scene);
            assert_eq!(current(), Subsystem::Scene);
            scoped(Subsystem::Assets, || assert_eq!(current(), Subsystem::Assets));
            assert_eq!(current(), Subsystem::Scene);
        }
        assert_eq!(current(), Subsystem::Unscoped);
        // Another thread starts unscoped whatever this one does.
        let _b = enter(Subsystem::Physics);
        let other = std::thread::spawn(current).join().ok();
        assert_eq!(other, Some(Subsystem::Unscoped));
    }

    #[test]
    fn ledger_counts_alloc_resize_and_free_against_the_owner() {
        // The ledger is process-wide and this test binary's other tests allocate concurrently
        // through the system allocator (not counted: no TaggingAllocator here); the Gameplay
        // slot is used by nothing else in this crate's tests.
        let tag = Subsystem::Gameplay as u8;
        let before = snapshot().subsystem(Subsystem::Gameplay);
        record_alloc(tag, 0, 100);
        record_resize(tag, 0, 100, 250);
        let mid = snapshot();
        assert!(mid.tracking);
        let g = mid.subsystem(Subsystem::Gameplay);
        assert_eq!(g.live_bytes, before.live_bytes + 250);
        assert_eq!(g.live_allocations, before.live_allocations + 1);
        assert!(g.peak_bytes >= g.live_bytes);
        record_resize(tag, 0, 250, 50);
        record_free(tag, 0, 50);
        let after = snapshot();
        assert_eq!(after.subsystem(Subsystem::Gameplay).live_bytes, before.live_bytes);
        let growth = mid.growth_since(&after);
        assert_eq!(growth.bytes(Subsystem::Gameplay), 250);
        assert!(mid.table().contains("gameplay"));
    }

    #[test]
    fn growth_names_the_largest_grower() {
        let mut growth = MemoryGrowth::default();
        assert_eq!(growth.largest(), None);
        growth.subsystems = std::array::from_fn(|i| (Subsystem::ALL[i], 0, 0));
        growth.subsystems[Subsystem::Scene as usize].1 = 4096;
        growth.subsystems[Subsystem::Assets as usize].1 = 64;
        growth.subsystems[Subsystem::Render as usize].1 = -8192;
        assert_eq!(growth.largest(), Some((Subsystem::Scene, 4096)));
    }

    #[test]
    fn leak_check_passes_flat_series_and_catches_a_small_steady_leak() {
        // Flat after warm-up, with a sawtooth (a buffer filling and emptying) and early growth.
        let mut flat = LeakCheck::new(100, 100);
        for cycle in 0..1000u64 {
            let warm = cycle.min(100) * 64;
            let saw = (cycle % 7) * 512;
            flat.sample(1_000_000 + warm + saw);
        }
        let v = flat.verdict(0).expect("ten windows");
        assert_eq!(v.cycles, 1000);
        assert_eq!(v.growth, 0, "{v:?}");
        assert!(v.bounded);

        // 16 bytes per cycle: one small allocation never freed.
        let mut leak = LeakCheck::new(100, 100);
        for cycle in 0..1000u64 {
            leak.sample(1_000_000 + cycle * 16);
        }
        let v = leak.verdict(8 * 1024).expect("ten windows");
        assert_eq!(v.growth, 800 * 16);
        assert!(!v.bounded, "{v:?}");
    }

    #[test]
    fn leak_check_needs_two_windows() {
        let mut check = LeakCheck::new(10, 10);
        for _ in 0..29 {
            check.sample(1);
        }
        assert!(check.verdict(0).is_none());
        check.sample(1);
        assert_eq!(check.verdict(0).map(|v| v.growth), Some(0));
    }
}
