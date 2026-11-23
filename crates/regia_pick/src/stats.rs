// SPDX-License-Identifier: MPL-2.0
//! Operation counters for the CPU pick path, used by the counting tests in
//! `tests/pick_counts.rs`. The counters are thread-local (parallel tests stay exact) and count
//! only when `debug_assertions` is on, so a release build pays nothing. Hosts should not call
//! the accessors; they exist to pin the complexity regressions the perf audit found.

thread_local! {
    static SKIN_EVALUATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static TRIANGLE_TESTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static MATRIX_INVERSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CACHE_SWEEPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static TERRAIN_SAMPLES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static TERRAIN_NODES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static ENTITY_VISITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static BVH_NODES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static SKIN_BUFFER_GROWS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static SCOPE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static SWEPT_BOUNDS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static SWEPT_BVH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[inline]
fn bump(cell: &'static std::thread::LocalKey<std::cell::Cell<u64>>) {
    if cfg!(debug_assertions) {
        cell.with(|c| c.set(c.get() + 1));
    }
}

/// One skinned vertex evaluated.
pub(crate) fn note_skin() {
    bump(&SKIN_EVALUATIONS);
}

/// One ray–triangle test.
pub(crate) fn note_triangle() {
    bump(&TRIANGLE_TESTS);
}

/// One `Mat4::inverse` on a model matrix.
pub(crate) fn note_inverse() {
    bump(&MATRIX_INVERSES);
}

/// One dead-entry sweep of a pick cache.
pub(crate) fn note_cache_sweep() {
    bump(&CACHE_SWEEPS);
}

/// One bilinear heightfield sample of the terrain march.
pub(crate) fn note_terrain_sample() {
    bump(&TERRAIN_SAMPLES);
}

/// One min/max-pyramid node visited by the terrain march.
pub(crate) fn note_terrain_node() {
    bump(&TERRAIN_NODES);
}

/// One entity visited by a world pick walk.
pub(crate) fn note_entity_visit() {
    bump(&ENTITY_VISITS);
}

/// One BVH node visited by a mesh cast.
pub(crate) fn note_bvh_node() {
    bump(&BVH_NODES);
}

/// The thread-local skin scratch buffer grew to fit a mesh.
pub(crate) fn note_skin_buffer_grow() {
    bump(&SKIN_BUFFER_GROWS);
}

/// One query-scope for the per-query dead-entry sweep. The pick caches release freed entries at
/// most once per query; nested scopes share the outermost one's flags.
pub struct PickCacheScope {
    _private: (),
}

/// Opens a pick cache scope: until the guard drops, each pick cache sweeps its dead entries at
/// most once. Queries that do not open a scope sweep on every call, as before.
#[must_use]
pub fn pick_cache_scope() -> PickCacheScope {
    SCOPE_DEPTH.with(|depth| {
        let next = depth.get().saturating_add(1);
        depth.set(next);
        if next == 1 {
            SWEPT_BOUNDS.with(|swept| swept.set(false));
            SWEPT_BVH.with(|swept| swept.set(false));
        }
    });
    PickCacheScope { _private: () }
}

impl Drop for PickCacheScope {
    fn drop(&mut self) {
        SCOPE_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Whether the bounds cache may sweep now (once per scope), marking it swept. Outside a
/// [`pick_cache_scope`] the legacy per-call sweep applies.
pub(crate) fn sweep_due_bounds() -> bool {
    if SCOPE_DEPTH.with(std::cell::Cell::get) == 0 {
        return true;
    }
    SWEPT_BOUNDS.with(|swept| {
        if swept.get() {
            false
        } else {
            swept.set(true);
            true
        }
    })
}

/// Whether the BVH cache may sweep now (once per scope), marking it swept. Outside a
/// [`pick_cache_scope`] the legacy per-call sweep applies.
pub(crate) fn sweep_due_bvh() -> bool {
    if SCOPE_DEPTH.with(std::cell::Cell::get) == 0 {
        return true;
    }
    SWEPT_BVH.with(|swept| {
        if swept.get() {
            false
        } else {
            swept.set(true);
            true
        }
    })
}

/// A snapshot of every counter.
#[doc(hidden)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PickCounts {
    /// Skinned vertices evaluated.
    pub skin_evaluations: u64,
    /// Ray–triangle tests.
    pub triangle_tests: u64,
    /// `Mat4::inverse` calls on model matrices.
    pub matrix_inverses: u64,
    /// Dead-entry sweeps of the pick caches.
    pub cache_sweeps: u64,
    /// Bilinear terrain samples.
    pub terrain_samples: u64,
    /// Min/max-pyramid nodes visited.
    pub terrain_nodes: u64,
    /// Entities visited by a world pick walk.
    pub entity_visits: u64,
    /// BVH nodes visited.
    pub bvh_nodes: u64,
    /// Growth events of the thread-local skin scratch buffer.
    pub skin_buffer_grows: u64,
}

fn read(cell: &'static std::thread::LocalKey<std::cell::Cell<u64>>) -> u64 {
    cell.with(std::cell::Cell::get)
}

/// Sets every counter to zero.
#[doc(hidden)]
#[must_use]
pub fn reset_counts() -> PickCounts {
    PickCounts {
        skin_evaluations: SKIN_EVALUATIONS.with(|c| c.replace(0)),
        triangle_tests: TRIANGLE_TESTS.with(|c| c.replace(0)),
        matrix_inverses: MATRIX_INVERSES.with(|c| c.replace(0)),
        cache_sweeps: CACHE_SWEEPS.with(|c| c.replace(0)),
        terrain_samples: TERRAIN_SAMPLES.with(|c| c.replace(0)),
        terrain_nodes: TERRAIN_NODES.with(|c| c.replace(0)),
        entity_visits: ENTITY_VISITS.with(|c| c.replace(0)),
        bvh_nodes: BVH_NODES.with(|c| c.replace(0)),
        skin_buffer_grows: SKIN_BUFFER_GROWS.with(|c| c.replace(0)),
    }
}

/// The current counter values.
#[doc(hidden)]
#[must_use]
pub fn counts() -> PickCounts {
    PickCounts {
        skin_evaluations: read(&SKIN_EVALUATIONS),
        triangle_tests: read(&TRIANGLE_TESTS),
        matrix_inverses: read(&MATRIX_INVERSES),
        cache_sweeps: read(&CACHE_SWEEPS),
        terrain_samples: read(&TERRAIN_SAMPLES),
        terrain_nodes: read(&TERRAIN_NODES),
        entity_visits: read(&ENTITY_VISITS),
        bvh_nodes: read(&BVH_NODES),
        skin_buffer_grows: read(&SKIN_BUFFER_GROWS),
    }
}
