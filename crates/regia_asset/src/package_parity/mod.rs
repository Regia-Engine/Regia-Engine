// SPDX-License-Identifier: MPL-2.0
//! Dev-run versus package-run parity (C-12, ADR-8428).
//!
//! `diff_paths` compares two asset-access traces. `open_trace` classifies a
//! package tree from strace text. Both are requirements rewrites; no shell or
//! Python is shipped.

mod open_trace;
mod trace;

pub use open_trace::{
    ChunkReport, ClassStats, FileClass, FileEntry, OpenReport, build_open_report, classify_text,
    open_trace_main,
};
pub use trace::{ParityReport, diff_paths, diff_paths_mode, gltf_memo_key};

#[cfg(test)]
mod tests;
