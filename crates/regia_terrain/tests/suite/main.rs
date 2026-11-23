// SPDX-License-Identifier: MPL-2.0
//! One test binary for the integration tests of `regia_terrain` that need no binary of their own (ADR-2986,
//! `docs/06-testing-and-ci.md` § 9): every module below was a `tests/<name>.rs` binary, and each test now
//! runs as `<name>::<test>`. Run one with `cargo test -p regia_terrain --test suite <name>::<test>`.
//!
//! A new test file still goes at `tests/<name>.rs` as its own binary by default. Move it here only when it
//! needs no process of its own: no `#[global_allocator]`, no environment or working-directory change, no
//! process-wide singleton, no GPU device, and no doc, script or registry that names its binary.

mod backdrop;
mod grid_origin;
mod noise_ramp;
mod rect_extent;
mod terrain_parity;
