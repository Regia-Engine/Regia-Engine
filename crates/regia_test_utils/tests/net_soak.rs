// SPDX-License-Identifier: MPL-2.0
//! The network soak (ADR-7899, `book/agents/net-soak.md`).
//!
//! `net_soak_matrix` is what `cargo xtask net-soak` runs: with `REGIA_NET_SOAK_OUT` set it runs the
//! requested plan (`regia_net_soak`) and writes `result.json` and `summary.md` there (xtask turns
//! failed bars into its exit code); without it, it is a no-op so the workspace test run stays
//! cheap. The harness's own tests live in `crates/regia_net_soak`.
//!
//! The binary installs the counting allocator so each run reports its peak heap growth.

use regia_net_soak as soak;
use regia_test_utils::alloc_counter::{self, CountingAlloc};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

#[test]
fn net_soak_matrix() {
    // Allocations per server tick (ADR-8454).
    soak::probe::install(alloc_counter::thread_allocs);
    if let Some(result) = soak::run_from_env(|run: &mut dyn FnMut()| alloc_counter::peak_bytes(run).1) {
        let report = result.unwrap_or_else(|e| panic!("net-soak harness error: {e}"));
        eprintln!("{}", report.summary_md());
    }
}
