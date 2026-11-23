// SPDX-License-Identifier: MPL-2.0
//! One test binary for the integration tests of `regia_physics` that need no binary of their own (ADR-2986,
//! `docs/06-testing-and-ci.md` § 9): every module below was a `tests/<name>.rs` binary, and each test now
//! runs as `<name>::<test>`. Run one with `cargo test -p regia_physics --test suite <name>::<test>`.
//!
//! A new test file still goes at `tests/<name>.rs` as its own binary by default. Move it here only when it
//! needs no process of its own: no `#[global_allocator]`, no environment or working-directory change, no
//! process-wide singleton, no GPU device, and no doc, script or registry that names its binary.

mod audit3_regressions;
mod cloth_spec55;
mod concurrency_1103;
mod contract_queries;
mod debris_integration;
mod destruction_props;
mod destruction_replica;
mod heightfield_holes;
mod m20_integration;
mod numerics_v035;
mod panics_1102;
mod parked_layers;
mod perf_20k;
mod physics2d_removal;
mod post05_api;
mod set_order;
mod shape_validation;
mod spawn_order_2d;
mod spec15_conformance;
mod spec15_finish;
mod stack2d;
mod swept_ccd;
mod terrain_collision_parity;
mod terrain_runtime_collider;
mod tq_cloth_edges;
mod tq_debris_edges;
mod tq_destruction_edges;
mod tq_layers_convert;
mod tq_platformer_edges;
mod tunnel_trimesh;
