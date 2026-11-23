// SPDX-License-Identifier: MPL-2.0
//! Telemetry taps for the lower layers: process-wide counters, gauges and a bounded queue of
//! notable moments that crates below `regia_telemetry` (assets, render, terrain, VFX) feed and
//! the telemetry plugin reads (`book/user-guide/telemetry.md`).
//!
//! Everything here is cheap enough to call on a hot path: a counter or gauge is one relaxed
//! atomic, and [`note`] copies its label into a fixed slot of a preallocated ring (no allocation;
//! when the ring is full the note is dropped and counted). Nothing here logs: the plugin decides
//! what becomes a `tracing` event, rate-limits it, and the host's subscriber decides where it goes.

use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

/// Monotonic process-wide counters (`add`, read with [`counter`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Counter {
    /// `get_current_texture` returned `Outdated` (the surface is reconfigured).
    SurfaceOutdated,
    /// `get_current_texture` returned `Lost` (the surface is recreated).
    SurfaceLost,
    /// `get_current_texture` timed out.
    SurfaceTimeout,
    /// `get_current_texture` returned `Occluded` or `Validation`.
    SurfaceOther,
    /// Streamed material texture sets shrunk under the resident budget (evictions).
    TextureEvictions,
    /// Asset loads finished (any kind).
    AssetLoads,
    /// wgpu uncaptured errors (validation, out of memory, internal).
    GpuErrors,
    /// Panics (the log panic hook counts them; `summary.json` `errors.panics`).
    Panics,
    /// GPU device losses that were not the app's own shutdown.
    DeviceLost,
    /// Audio output suspends (every kind).
    AudioSuspends,
    /// Audio output suspends caused by sustained slow frames.
    AudioThrottleSuspends,
}

impl Counter {
    /// Number of counters.
    pub const COUNT: usize = 11;

    /// Stable snake-case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::SurfaceOutdated => "surface_outdated",
            Self::SurfaceLost => "surface_lost",
            Self::SurfaceTimeout => "surface_timeout",
            Self::SurfaceOther => "surface_other",
            Self::TextureEvictions => "texture_evictions",
            Self::AssetLoads => "asset_loads",
            Self::GpuErrors => "gpu_errors",
            Self::Panics => "panics",
            Self::DeviceLost => "device_lost",
            Self::AudioSuspends => "audio_suspends",
            Self::AudioThrottleSuspends => "audio_throttle_suspends",
        }
    }
}

/// Last-value gauges (`set_gauge`, read with [`gauge`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Gauge {
    /// Scatter instances (rocks, props) handed to the last frame.
    ScatterEmitted,
    /// Estimated live particles across VFX instances after the last update.
    ParticlesLive,
}

impl Gauge {
    /// Number of gauges.
    pub const COUNT: usize = 2;

    /// Stable snake-case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::ScatterEmitted => "scatter_emitted",
            Self::ParticlesLive => "particles_live",
        }
    }
}

static COUNTERS: [AtomicU64; Counter::COUNT] = [const { AtomicU64::new(0) }; Counter::COUNT];
static GAUGES: [AtomicU64; Gauge::COUNT] = [const { AtomicU64::new(0) }; Gauge::COUNT];

/// Adds `n` to `counter`.
#[inline]
pub fn add(counter: Counter, n: u64) {
    COUNTERS[counter as usize].fetch_add(n, Ordering::Relaxed);
}

/// `counter`'s total since the process started.
#[inline]
#[must_use]
pub fn counter(counter: Counter) -> u64 {
    COUNTERS[counter as usize].load(Ordering::Relaxed)
}

/// Sets `gauge` to `value`.
#[inline]
pub fn set_gauge(gauge: Gauge, value: u64) {
    GAUGES[gauge as usize].store(value, Ordering::Relaxed);
}

/// `gauge`'s last value (0 before anything set it).
#[inline]
#[must_use]
pub fn gauge(gauge: Gauge) -> u64 {
    GAUGES[gauge as usize].load(Ordering::Relaxed)
}

/// Bytes of a note's label; longer labels are cut at a character boundary.
pub const NOTE_LABEL_BYTES: usize = 120;
/// Notes the ring holds between two drains (the plugin drains once per frame).
pub const NOTE_CAPACITY: usize = 64;

/// What a [`Note`] is about. The plugin turns each into a `regia::stream` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoteKind {
    /// An asset load took longer than [`SLOW_ASSET_LOAD_MS`] (`value` = ms).
    SlowAssetLoad,
    /// A pak entry failed its integrity check (`PakError::Corrupt` / hash mismatch; `value` =
    /// byte offset of the entry, or -1 when unknown).
    PakCorrupt,
}

impl NoteKind {
    /// Stable snake-case name (the event's `kind` field).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::SlowAssetLoad => "slow_asset_load",
            Self::PakCorrupt => "pak_corrupt",
        }
    }
}

/// An asset load slower than this is noted ([`NoteKind::SlowAssetLoad`]).
pub const SLOW_ASSET_LOAD_MS: f64 = 100.0;

/// One noted moment, copied out by [`drain_notes`].
#[derive(Clone, Copy)]
pub struct Note {
    /// What happened.
    pub kind: NoteKind,
    /// Kind-specific number (see [`NoteKind`]).
    pub value: f64,
    label: [u8; NOTE_LABEL_BYTES],
    label_len: u8,
}

impl Note {
    const EMPTY: Self = Self {
        kind: NoteKind::SlowAssetLoad,
        value: 0.0,
        label: [0; NOTE_LABEL_BYTES],
        label_len: 0,
    };

    /// The label (asset id and kind, pak path and entry), possibly cut short.
    #[must_use]
    pub fn label(&self) -> &str {
        std::str::from_utf8(&self.label[..usize::from(self.label_len)]).unwrap_or("")
    }
}

impl std::fmt::Debug for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Note")
            .field("kind", &self.kind)
            .field("value", &self.value)
            .field("label", &self.label())
            .finish()
    }
}

struct NoteRing {
    slots: [Note; NOTE_CAPACITY],
    len: usize,
}

static NOTES: Mutex<NoteRing> = Mutex::new(NoteRing {
    slots: [Note::EMPTY; NOTE_CAPACITY],
    len: 0,
});
static NOTES_PENDING: AtomicU64 = AtomicU64::new(0);
static NOTES_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Records a notable moment. Never blocks for long and never allocates: when the ring is full
/// (nothing drained it) the note is dropped and counted ([`notes_dropped`]).
pub fn note(kind: NoteKind, label: &str, value: f64) {
    let mut cut = label.len().min(NOTE_LABEL_BYTES);
    while !label.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut ring = NOTES.lock();
    if ring.len >= NOTE_CAPACITY {
        drop(ring);
        NOTES_DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let i = ring.len;
    let slot = &mut ring.slots[i];
    slot.kind = kind;
    slot.value = value;
    slot.label[..cut].copy_from_slice(&label.as_bytes()[..cut]);
    slot.label_len = cut as u8;
    ring.len += 1;
    NOTES_PENDING.store(1, Ordering::Release);
}

/// `true` when [`drain_notes`] has something to hand out (one atomic load).
#[inline]
#[must_use]
pub fn notes_pending() -> bool {
    NOTES_PENDING.load(Ordering::Acquire) != 0
}

/// Copies up to `out.len()` notes into `out` (oldest first), removes them from the ring and
/// returns how many were copied.
pub fn drain_notes(out: &mut [Note]) -> usize {
    if !notes_pending() {
        return 0;
    }
    let mut ring = NOTES.lock();
    let n = ring.len.min(out.len());
    out[..n].copy_from_slice(&ring.slots[..n]);
    let len = ring.len;
    ring.slots.copy_within(n..len, 0);
    ring.len -= n;
    if ring.len == 0 {
        NOTES_PENDING.store(0, Ordering::Release);
    }
    n
}

/// A blank note buffer for [`drain_notes`].
#[must_use]
pub fn note_buffer() -> [Note; NOTE_CAPACITY] {
    [Note::EMPTY; NOTE_CAPACITY]
}

/// Notes dropped because the ring was full.
#[must_use]
pub fn notes_dropped() -> u64 {
    NOTES_DROPPED.load(Ordering::Relaxed)
}

/// Times a scope and notes it as a [`NoteKind::SlowAssetLoad`] when it runs longer than
/// [`SLOW_ASSET_LOAD_MS`]; also counts [`Counter::AssetLoads`]. `let _t = SlowLoad::start("mesh", id);`
#[must_use = "the load is timed until the guard drops"]
pub struct SlowLoad<'a> {
    kind: &'static str,
    what: &'a dyn std::fmt::Display,
    start: std::time::Instant,
}

impl<'a> SlowLoad<'a> {
    /// Starts timing a load of `kind` (mesh, texture, …) for `what` (usually the asset id).
    pub fn start(kind: &'static str, what: &'a dyn std::fmt::Display) -> Self {
        Self {
            kind,
            what,
            start: std::time::Instant::now(),
        }
    }
}

impl SlowLoad<'_> {
    /// Counts the load and notes it when `ms` is over [`SLOW_ASSET_LOAD_MS`] (the seam `drop` uses,
    /// so tests need no real delay).
    fn record(&self, ms: f64) {
        add(Counter::AssetLoads, 1);
        if ms > SLOW_ASSET_LOAD_MS {
            // A slow load already cost > 100 ms; formatting its label is noise next to that.
            let label = format!("{} {}", self.kind, self.what);
            note(NoteKind::SlowAssetLoad, &label, ms);
        }
    }
}

impl Drop for SlowLoad<'_> {
    fn drop(&mut self) {
        self.record(self.start.elapsed().as_secs_f64() * 1000.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_gauges_and_notes() {
        let before = counter(Counter::TextureEvictions);
        add(Counter::TextureEvictions, 3);
        assert!(counter(Counter::TextureEvictions) >= before + 3);
        set_gauge(Gauge::ParticlesLive, 42);
        assert_eq!(gauge(Gauge::ParticlesLive), 42);

        let mut buf = note_buffer();
        drain_notes(&mut buf);
        let long = "é".repeat(100);
        note(NoteKind::PakCorrupt, &long, 7.0);
        assert!(notes_pending());
        let n = drain_notes(&mut buf);
        assert!(n >= 1);
        let ours = buf[..n]
            .iter()
            .find(|n| n.kind == NoteKind::PakCorrupt)
            .map(|n| n.label().len());
        assert!(ours.is_some_and(|l| l <= NOTE_LABEL_BYTES && l % 2 == 0));
        let dropped = notes_dropped();
        for _ in 0..NOTE_CAPACITY + 5 {
            note(NoteKind::SlowAssetLoad, "x", 1.0);
        }
        assert!(notes_dropped() >= dropped + 5);
        while drain_notes(&mut buf) > 0 {}
        assert!(!notes_pending());
        slow_load_counts_and_notes_over_threshold();
    }

    /// Run from `counters_gauges_and_notes`: the note ring is process-wide, so the drains of the
    /// two must not interleave.
    fn slow_load_counts_and_notes_over_threshold() {
        let mut buf = note_buffer();
        while drain_notes(&mut buf) > 0 {}
        let loads = counter(Counter::AssetLoads);
        let guard = SlowLoad::start("mesh", &"fast_asset");
        guard.record(SLOW_ASSET_LOAD_MS);
        assert!(counter(Counter::AssetLoads) > loads, "every load is counted");
        assert!(!notes_pending(), "exactly the threshold is not slow");
        guard.record(SLOW_ASSET_LOAD_MS + 0.5);
        let n = drain_notes(&mut buf);
        let slow = buf[..n].iter().find(|n| n.kind == NoteKind::SlowAssetLoad);
        assert!(slow.is_some_and(|n| n.label() == "mesh fast_asset" && n.value > SLOW_ASSET_LOAD_MS));
        // The real drop path of an immediate guard notes nothing.
        drop(guard);
        while drain_notes(&mut buf) > 0 {}
    }
}
