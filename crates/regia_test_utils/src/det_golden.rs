// SPDX-License-Identifier: MPL-2.0
//! The deterministic golden harness (ADR-8046, plan row F-02), re-exported from
//! `regia_detsim::golden`.
//!
//! The harness lives in the lockstep kit so that a game's sim crate, and the cross-built test
//! binaries the determinism runners ship to aarch64 hosts, link it without the renderer this crate
//! depends on. Tests that already use `regia_test_utils` reach it here; see the book page
//! "Determinism goldens".
//!
//! ```
//! use regia_test_utils::det_golden::{scenarios, DetRegistry, GoldenMode};
//!
//! // Every engine scenario is registered once (the list grows as lanes add scenarios).
//! let registry = DetRegistry::with_engine_scenarios();
//! assert_eq!(registry.entries().len(), scenarios::engine_scenarios().len());
//! assert!(registry.get("steer_slalom").is_some());
//! # let _ = GoldenMode::Check;
//! ```

pub use regia_detsim::golden::*;
