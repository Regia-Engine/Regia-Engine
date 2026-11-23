// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets of the spec 23 gap parsers (ADR-2346): the MCP HTTP request head and admission
//! policy (`regia mcp --http`), `cargo test` output (`agent.run_tests`) and the staged-review queue
//! file (`.regia/agent/review/queue.json`).

use super::formats::text;
use super::{FuzzInput, FuzzTarget};

/// A `cargo test` run: banner, outcomes, a failure section, the summary and an error line.
const RUN_TESTS_FIXTURE: &str = "\
     Running unittests src/lib.rs (target/debug/deps/demo-0123abcd)

running 2 tests
test tests::adds ... ok
test tests::breaks ... FAILED

failures:

---- tests::breaks stdout ----
assertion failed

failures:
    tests::breaks

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

error: test failed, to rerun pass `--lib`
";

/// Feeds every line to the tally, read as the worker reads cargo's pipes (lossy UTF-8, so any
/// bytes are output). Test output is a stream: a run cut short is valid input, so the target is
/// cut-tolerant (`CUT_TOLERANT` in `tests/fuzz_targets.rs`). No writer to round-trip.
fn run_tests_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let text = String::from_utf8_lossy(bytes);
    if text.trim().is_empty() {
        return Err("no cargo test output".into());
    }
    let mut tally = crate::run_tests::RunTally::default();
    for line in text.lines() {
        tally.feed(line);
    }
    if tally.failed as usize != tally.failed_names.len() {
        return Err(format!("{}failed count and names disagree", super::BUG_PREFIX));
    }
    Ok(Vec::new())
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        FuzzTarget {
            name: "agent.mcp_http_request",
            format: None,
            format_version: 0,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: crate::mcp_http::fuzz_fixture,
                load: crate::mcp_http::fuzz_load,
            },
        },
        FuzzTarget {
            name: "agent.run_tests_output",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: || RUN_TESTS_FIXTURE.as_bytes().to_vec(),
                load: run_tests_load,
            },
        },
        // `agent.cook` / `agent.package`'s regia --json output (ADR-7217): any bytes, cut is valid.
        FuzzTarget {
            name: "agent.cook_package_output",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: || crate::cook_package::FUZZ_FIXTURE.as_bytes().to_vec(),
                load: crate::cook_package::fuzz_load,
            },
        },
        // `agent.build_game`'s cargo JSON stream (ADR-5707): any bytes, a cut stream is valid.
        FuzzTarget {
            name: "agent.build_game_output",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: || crate::build_game::FUZZ_FIXTURE.as_bytes().to_vec(),
                load: crate::build_game::fuzz_load,
            },
        },
        text(
            "agent.review_queue",
            None,
            crate::review::REVIEW_QUEUE_FORMAT_VERSION,
            true,
            crate::review::fuzz_fixture,
            crate::review::fuzz_load,
        ),
        // The test-source scan of `agent.golden_test` (ADR-2821): any bytes, a cut source is valid.
        FuzzTarget {
            name: "agent.golden_test_source",
            format: None,
            format_version: 0,
            round_trip: false,
            input: FuzzInput::Bytes {
                fixture: crate::golden_tools::fuzz_source_fixture,
                load: crate::golden_tools::fuzz_scan_source,
            },
        },
    ]
}
