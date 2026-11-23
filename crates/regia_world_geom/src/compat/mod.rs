// SPDX-License-Identifier: MPL-2.0
//! Stage A: Direct Order 27's tunnel code, copied with its arithmetic unchanged (spec 121 § 4,
//! ADR-8012 A-1, ADR-8050).
//!
//! The item names are DO27's: they are the switch contract (DO27 swaps its modules for these and runs
//! its pins). Each file names its DO27 source, commit and line ranges in its header, and has a
//! `docs/ported-sources.toml` row: `layout`, `pack` and the fixtures come from tag regia-absorb-1
//! (`d6ed37677`); the T3 query modules (`mesh`, `ground`, `sampler`, `surface`, `state`, `hover_ground`,
//! `sense`) from tag regia-absorb-2 (`e6c84ba6d`, ADR-8167), and the on-foot grid (`onfoot`) from the same tag (ADR-8306). Stage B (the native core) replaces these after DO27's one
//! sim-version bump (spec 121 § 14).

pub mod ground;
pub mod layout;
pub mod mesh;
pub mod pack;
pub mod sampler;
pub mod terrain_patch;

pub mod hover_ground;
pub mod onfoot;
pub mod sense;
pub(crate) mod source_math;
pub mod state;
pub mod surface;

#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;

#[cfg(test)]
mod tunnels_tests;
