// SPDX-License-Identifier: MPL-2.0
//! World streaming runtime (`docs/specs/78-world-streaming.md`, ADR-1804, ADR-1888).
//!
//! A level larger than what fits in memory is cooked into square cells. At run time streaming
//! sources (cameras, players, scripted pins) pull nearby cells in and let far cells go, within a
//! per-frame time budget and a memory budget, without ever blocking the frame on IO.
//!
//! - [`format`](mod@format): the cooked world index (`.wstream`) and cell blobs (`.wcell`).
//! - [`source`]: where cell bytes come from, tickets and cancel tokens.
//! - [`WorldStreaming`]: the scheduler, one record per open streamed level.
//! - [`origin`]: the floating origin and the rebase of everything the ECS holds.
//! - [`open_streamed_level`]: opens a cooked streamed level.
//!
//! [`WorldStreamingPlugin`] adds the systems; it is inert until a streamed level opens. Subsystems
//! never depend on this crate: they register gates in `regia_scene::streaming::StreamGates` and read
//! the messages of `regia_scene::streaming`. See `book/agents/streaming-runtime.md`.
//!
//! # Getting started
//!
//! Add the plugin after the scene plugin. With no streamed level open the runtime is inert, and
//! every cell of a level has a stable asset id derived from the level's id:
//!
//! ```
//! use regia_app::prelude::*;
//! use regia_core::id::Guid;
//! use regia_scene::ScenePlugin;
//! use regia_scene::level::LevelId;
//! use regia_scene::stream_grid::CellCoord;
//! use regia_world::format::{cell_blob_id, world_index_id};
//! use regia_world::{WorldStreaming, WorldStreamingPlugin};
//!
//! let mut app = App::new();
//! app.add_plugins(ScenePlugin).add_plugins(WorldStreamingPlugin);
//!
//! // No level is open yet.
//! let streaming = app.world().resource::<WorldStreaming>();
//! assert!(!streaming.is_active());
//! assert_eq!(streaming.levels().count(), 0);
//!
//! // The cooked files of a level are addressed by id, never by path.
//! let level = LevelId(Guid::from_u128(0xfeed));
//! let cell = CellCoord::new(2, -1);
//! assert_ne!(world_index_id(level), cell_blob_id(level, cell));
//! assert_eq!(cell_blob_id(level, cell), cell_blob_id(level, CellCoord::new(2, -1)));
//! ```

pub mod format;
pub mod hlod;
pub mod hlod_fade;
pub mod hlod_voxels;
pub mod open;
pub mod origin;
pub mod runtime;
pub mod source;
mod systems;

pub use open::{StreamOpenError, open_if_cooked, open_streamed_from, open_streamed_level};
pub use origin::WorldOrigin;
pub use regia_scene::floating_origin::Rebase;
pub use regia_scene::stream_plan::{SourceId, SourceView};
pub use regia_scene::streaming::{
    CellFailed, CellLoaded, CellReady, CellUnloading, OriginShifted, Readiness, StreamBudgets, StreamGates,
    StreamSet, StreamViews,
};
pub use runtime::stream_cells_set;
pub use runtime::{
    CellState, Pin, PinId, PinWait, StreamRuntimeError, StreamStats, StreamTiming, StreamTrace, TraceEvent,
    TraceKind, WorldStreaming,
};
pub use source::{
    CancelToken, CellReadError, CellSink, CellSource, MemoryCellSource, PayloadCellSource, Ticket,
};

/// Counts heap allocations on this thread. `cargo test --lib` compiles this crate into the test
/// binary, so the allocator installed below is the one those tests run under. Integration test
/// binaries do not use it.
#[cfg(test)]
pub(crate) mod alloc_count {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    /// Forwards to the system allocator and counts this thread's allocations.
    pub struct Counting;

    thread_local! {
        static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    }

    // The workspace denies `unsafe_code`. This impl exists only in the unit-test binary.
    #[allow(unsafe_code)]
    // SAFETY: `GlobalAlloc` is an unsafe trait. Each method forwards to `System`, and the caller
    // upholds that method's contract. The counter bump does not unwind into the allocator.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = ALLOCATIONS.try_with(|count| count.set(count.get().saturating_add(1)));
            // SAFETY: the caller upholds `GlobalAlloc::alloc`.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: the caller upholds `GlobalAlloc::dealloc`.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let _ = ALLOCATIONS.try_with(|count| count.set(count.get().saturating_add(1)));
            // SAFETY: the caller upholds `GlobalAlloc::realloc`.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    /// Allocations this thread has made since the process started.
    pub fn allocations() -> u64 {
        ALLOCATIONS.with(Cell::get)
    }

    /// The smallest allocation count of `times` runs of `body`. A count that happens on every run
    /// stays in the minimum; a count from another test scheduled on this thread between runs does not.
    pub fn least(times: usize, mut body: impl FnMut()) -> u64 {
        let mut least = u64::MAX;
        for _ in 0..times {
            let before = allocations();
            body();
            least = least.min(allocations().saturating_sub(before));
        }
        least
    }
}

#[cfg(test)]
#[global_allocator]
static REGIA_WORLD_TEST_COUNTING_ALLOCATOR: alloc_count::Counting = alloc_count::Counting;

use regia_app::prelude::*;
use regia_app::{OnLevelUnloading, PostUpdate, PostUpdateSet, PreUpdate};
use regia_scene::level::{LevelLoaded, LevelLoading, LevelUnloading};

/// Adds the streaming resources and systems. Part of `regia::DefaultPlugins`; add it after the scene
/// plugin. Inert without a streamed level.
#[derive(Debug, Default)]
pub struct WorldStreamingPlugin;

impl Plugin for WorldStreamingPlugin {
    fn build(&self, app: &mut App) {
        regia_scene::streaming::add_to_app(app);
        app.add_message::<LevelLoading>()
            .add_message::<LevelLoaded>()
            .add_message::<LevelUnloading>();
        app.init_resource::<WorldStreaming>()
            .init_resource::<StreamStats>()
            .init_resource::<WorldOrigin>()
            .register_type::<StreamStats>()
            .init_resource::<regia_scene::streaming::OriginOffset>()
            .insert_resource(regia_scene::streaming::StreamedLevelOpener(open::open_if_cooked));
        app.configure_sets(
            FixedUpdate,
            (
                StreamSet::Guard,
                StreamSet::Sources,
                StreamSet::Rebase,
                StreamSet::Plan,
                StreamSet::Complete,
                StreamSet::Apply,
            )
                .chain(),
        );
        // `Guard` always runs: it counts the step and wakes bodies after a level closed.
        app.add_systems(
            FixedUpdate,
            (
                systems::sync_streaming_active,
                systems::publish_layouts,
                systems::guard,
            )
                .chain()
                .in_set(StreamSet::Guard),
        );
        // Also without a streamed level: the last one to close leaves the origin to bring home.
        app.add_systems(
            FixedUpdate,
            (origin::return_origin_home, origin::publish_origin_offset)
                .chain()
                .after(origin::rebase_step)
                .in_set(StreamSet::Rebase),
        );
        app.add_systems(
            FixedUpdate,
            (
                systems::sample_sources.in_set(StreamSet::Sources),
                origin::rebase_step.in_set(StreamSet::Rebase),
                (systems::run_teleports, systems::plan)
                    .chain()
                    .in_set(StreamSet::Plan),
                systems::complete_fixed.in_set(StreamSet::Complete),
                systems::apply_fixed.in_set(StreamSet::Apply),
            )
                .run_if(systems::streaming_active),
        );
        app.add_systems(OnLevelUnloading, systems::close_unloading_levels);
        app.add_systems(OnLevelUnloading, open::forget_replicated_tables);
        app.add_systems(PreUpdate, systems::watch_suspended);
        app.add_systems(Lifecycle, systems::watch_memory_trim);
        app.add_systems(
            PreUpdate,
            systems::complete_frame
                .in_set(StreamSet::Complete)
                .run_if(systems::streaming_active),
        );
        app.add_systems(
            Update,
            systems::apply_frame
                .in_set(StreamSet::Apply)
                .run_if(systems::streaming_active),
        );
        app.configure_sets(
            PostUpdate,
            StreamSet::Hlod.after(PostUpdateSet::TransformPropagate),
        );
        app.add_systems(
            PostUpdate,
            systems::hlod_step
                .in_set(StreamSet::Hlod)
                .run_if(systems::streaming_active),
        );
    }
}
