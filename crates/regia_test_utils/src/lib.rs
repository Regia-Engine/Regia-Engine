// SPDX-License-Identifier: MPL-2.0
//! Headless rendering harness, golden-image comparison, test worlds.
//!
//! Specification: `docs/06-testing-and-ci.md`.

/// The umbrella crate, re-exported so integration tests in other crates can reach the engine
/// prelude (`World`, `App`, `MinimalPlugins`, `AppTypeRegistry`, …) through this test-only
/// dependency without adding a direct dev-dependency on `regia`.
pub use regia;

pub mod book;
pub mod det_matrix;
pub mod functional;
pub mod fuzz;
pub mod gi_error;
pub mod gi_scenes;
pub mod golden;
pub mod hwrt;
pub mod process;
pub mod replay_soak;
pub mod repro;
pub mod scene_loader;
pub mod scenes;
pub mod shuffle;
pub mod spec27;
pub mod temp;
// The per-subsystem heap ledger in a test binary (ADR-2987).
pub mod heap_ledger;
// The one counting global allocator and the steady-state rule of the allocation tests (ADR-5718).
pub mod alloc_counter;
// wgpu's usage-scope pool filled on purpose, for the steady-frame allocation tests (ADR-5718).
pub mod device_pools;
// One test at a time on the process's one headless device (ADR-7483).
pub mod serial_gpu;

pub use temp::{TempDir, unique_temp_dir, write_atomic};
// Book tutorial pages as doc tests: their `sh` fence steps, read at run time (ADR-9810).
pub mod tutorial;
// The deterministic golden harness, re-exported from the lockstep kit (ADR-8046, plan row F-02).
pub mod det_golden;
