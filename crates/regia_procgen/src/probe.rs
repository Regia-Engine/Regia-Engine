// SPDX-License-Identifier: MPL-2.0
//! Thread-local counters for performance regression tests.
//!
//! Each counter stays at zero until [`arm`] on this thread. A production call pays one
//! thread-local branch and does not allocate. Parallel tests do not share counts.

use std::cell::Cell;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static VALUE_CALLS: Cell<u64> = const { Cell::new(0) };
    static CLAIMS_CALLS: Cell<u64> = const { Cell::new(0) };
    static CONTAINS_CALLS: Cell<u64> = const { Cell::new(0) };
    static GRAPH_READS: Cell<u64> = const { Cell::new(0) };
    static STAMP_CLONES: Cell<u64> = const { Cell::new(0) };
    static WEIGHT_VECS: Cell<u64> = const { Cell::new(0) };
    static LAYER_WARNS: Cell<u64> = const { Cell::new(0) };
    static MASK_KEYS: Cell<u64> = const { Cell::new(0) };
    static FLAT_VECS: Cell<u64> = const { Cell::new(0) };
    static IDLE_SCANS: Cell<u64> = const { Cell::new(0) };
    static COUNT_FLUSHES: Cell<u64> = const { Cell::new(0) };
}

fn bump(slot: &'static std::thread::LocalKey<Cell<u64>>) {
    if !ARMED.with(Cell::get) {
        return;
    }
    slot.with(|c| c.set(c.get().saturating_add(1)));
}

/// Zeros every counter on this thread and starts counting.
pub fn arm() {
    ARMED.with(|c| c.set(true));
    for slot in [
        &VALUE_CALLS,
        &CLAIMS_CALLS,
        &CONTAINS_CALLS,
        &GRAPH_READS,
        &STAMP_CLONES,
        &WEIGHT_VECS,
        &LAYER_WARNS,
        &MASK_KEYS,
        &FLAT_VECS,
        &IDLE_SCANS,
        &COUNT_FLUSHES,
    ] {
        slot.with(|c| c.set(0));
    }
}

/// Stops counting on this thread. The last counts stay readable.
pub fn disarm() {
    ARMED.with(|c| c.set(false));
}

/// Pauses counting on this thread until the guard drops (then the previous state returns).
///
/// The evaluator holds one around every pool task it runs (ADR-7252): a thread waiting on a pool
/// scope runs other tasks meanwhile, possibly another test's, and those must not count toward the
/// waiting test. Counts therefore cover the work done inline on the armed thread only.
pub(crate) fn pause() -> Paused {
    Paused(ARMED.with(|c| c.replace(false)))
}

/// Restores this thread's counting state when dropped (see [`pause`]).
pub(crate) struct Paused(bool);

impl Drop for Paused {
    fn drop(&mut self) {
        let armed = self.0;
        ARMED.with(|c| c.set(armed));
    }
}

/// `BiomeAreaSnap::value` calls since [`arm`].
pub fn value_calls() -> u64 {
    VALUE_CALLS.with(Cell::get)
}

/// `claims_at` calls since [`arm`].
pub fn claims_calls() -> u64 {
    CLAIMS_CALLS.with(Cell::get)
}

/// `ExclusionRegion::contains` calls since [`arm`].
pub fn contains_calls() -> u64 {
    CONTAINS_CALLS.with(Cell::get)
}

/// `read_graph_text` calls since [`arm`].
pub fn graph_reads() -> u64 {
    GRAPH_READS.with(Cell::get)
}

/// Source `PointSet` clones inside `points.stamp` since [`arm`].
pub fn stamp_clones() -> u64 {
    STAMP_CLONES.with(Cell::get)
}

/// Heap weight vectors built by `BiomeWorld::layer_weight_at` since [`arm`].
pub fn weight_vecs() -> u64 {
    WEIGHT_VECS.with(Cell::get)
}

/// Missing-terrain-layer warnings formatted since [`arm`].
pub fn layer_warns() -> u64 {
    LAYER_WARNS.with(Cell::get)
}

/// Mask-cache key pairs allocated since [`arm`].
pub fn mask_keys() -> u64 {
    MASK_KEYS.with(Cell::get)
}

/// Per-point `Vec`s built by `points.flat_only` since [`arm`].
pub fn flat_vecs() -> u64 {
    FLAT_VECS.with(Cell::get)
}

/// Entities walked by the idle staleness scan since [`arm`].
pub fn idle_scans() -> u64 {
    IDLE_SCANS.with(Cell::get)
}

/// Per-name `NodeCtx::count` flushes from `biome.spawn` and `biome.exclude` since [`arm`].
pub fn count_flushes() -> u64 {
    COUNT_FLUSHES.with(Cell::get)
}

#[inline]
pub(crate) fn hit_value() {
    bump(&VALUE_CALLS);
}

#[inline]
pub(crate) fn hit_claims() {
    bump(&CLAIMS_CALLS);
}

#[inline]
pub(crate) fn hit_contains() {
    bump(&CONTAINS_CALLS);
}

#[inline]
pub(crate) fn hit_graph_read() {
    bump(&GRAPH_READS);
}

#[inline]
pub(crate) fn hit_layer_warn() {
    bump(&LAYER_WARNS);
}

#[inline]
pub(crate) fn hit_mask_key() {
    bump(&MASK_KEYS);
}

#[inline]
pub(crate) fn hit_idle_scan() {
    bump(&IDLE_SCANS);
}

#[inline]
pub(crate) fn hit_count_flush() {
    bump(&COUNT_FLUSHES);
}
