// SPDX-License-Identifier: MPL-2.0
//! `.netcapture` against the old-bytes corpus (ADR-2743, ADR-8221, ADR-8890):
//! `crates/regia_agent/tests/fixtures/format-corpus/netcapture/`. The fuzz-target registry cannot reach the
//! reader (`COVERED_ELSEWHERE`), so the four checks run here: every fixture loads at the current version, its
//! re-encoding reads back equal, a second pass changes nothing, and a copy patched to the next version is refused
//! with `capture_version`.

#[path = "../../regia_agent/tests/format_corpus_support/mod.rs"]
mod format_corpus_support;

use regia_net::capture::{CAPTURE_FORMAT_VERSION, CaptureFile, CaptureRecord, CaptureStats};

/// A small capture with every record kind, built from constants (the frozen current-version fixture).
fn example() -> CaptureFile {
    let records = vec![
        CaptureRecord::Session {
            game: "regia.corpus.netcapture".into(),
            protocol: 0x0123_4567_89ab_cdef,
        },
        CaptureRecord::Receive { at_ms: 1_000 },
        CaptureRecord::Flush { at_ms: 1_000 },
        CaptureRecord::Receive { at_ms: 1_016 },
        CaptureRecord::Deny {
            datagram: vec![3, 1, 2, 3],
        },
        CaptureRecord::Accept {
            sealed: true,
            datagram: vec![2, 9, 8, 7, 6, 5],
        },
        CaptureRecord::Packet {
            bytes: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        },
        CaptureRecord::Input {
            tick: 42,
            bytes: vec![1, 0],
        },
        CaptureRecord::Flush { at_ms: 1_016 },
        CaptureRecord::Frame {
            at_ms: 1_032,
            digest: 0xfeed_beef_0bad_f00d,
        },
        CaptureRecord::Goodbye { reason: Some(4) },
        CaptureRecord::Ended {
            code: "server_shutdown".into(),
        },
    ];
    CaptureFile {
        label: "format corpus: every record kind".into(),
        stats: CaptureStats::default(),
        records,
    }
}

fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    CaptureFile::from_bytes(bytes)
        .map(|f| f.to_bytes())
        .map_err(|e| e.to_string())
}

#[test]
fn bless_missing_current_fixtures() {
    if !format_corpus_support::blessing() {
        return;
    }
    format_corpus_support::bless_current(
        "netcapture",
        CAPTURE_FORMAT_VERSION,
        "bin",
        &example().to_bytes(),
        "built by crates/regia_net/tests/format_corpus.rs `example()`: every record kind of format_version 1",
    );
}

#[test]
fn every_netcapture_fixture_loads_round_trips_and_refuses_the_future() {
    let problems = format_corpus_support::check_format("netcapture", CAPTURE_FORMAT_VERSION, &load);
    assert!(problems.is_empty(), "{problems:#?}");
}

/// The example reads back equal through the reader, so the fixture the bless writes is a whole file.
#[test]
fn the_example_reads_back_equal() {
    let file = example();
    let back = CaptureFile::from_bytes(&file.to_bytes()).expect("reads");
    assert_eq!(back.label, file.label);
    assert_eq!(back.records, file.records);
}
