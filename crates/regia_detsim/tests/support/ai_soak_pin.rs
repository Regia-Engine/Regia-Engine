// SPDX-License-Identifier: MPL-2.0
//! The pinned AI soak fingerprint (ADR-8561), shared by `scale_soak.rs` and `det_golden.rs`.
//!
//! Default `AiSoakConfig` (900 units, 40 seats, 4 teams) and its default run: seed
//! `5eed_a100_0900`, 600 ticks at 60 Hz. Recorded on x86_64-linux; the aarch64 legs run it
//! through `cargo xtask det-golden` (the `det_golden` test target). A change here is a
//! simulation change: write why in the commit message.

/// `golden::record(..).run_hash()` of the default 600-tick run.
pub const AI_SOAK_900_RUN_HASH: u64 = 0x3f34_b790_c3e5_3fc8;
