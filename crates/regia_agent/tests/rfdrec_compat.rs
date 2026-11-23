// SPDX-License-Identifier: MPL-2.0
//! `.rfdrec` compatibility against frozen fixtures (`tests/fixtures/formats/rfdrec/`).
//!
//! `v1.rfdrec` was written once by a script that shares no code with the engine's writer. The test
//! loads it, compares a text dump with `v1.golden.txt`, re-serializes it byte for byte, and checks
//! that a newer-version file is refused with `replay_corrupt` naming both versions. Fixtures are
//! frozen by length and FNV-1a hash; a missing golden is written only with
//! `REGIA_BLESS_FORMAT_FIXTURES=1`, an existing file never.
#![cfg(feature = "dcc-feed")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;
use std::path::PathBuf;

use regia_agent::dcc_feed::recording::{Recording, RecordingError};
use regia_agent::dcc_feed::wire::Limits;

const FIXTURES: [(&str, usize, u64); 3] = [
    ("v1.rfdrec", 157, 0x4413_f3eb_03b8_a909),
    ("v1_empty.rfdrec", 24, 0x0e99_c8a7_18f2_9b7f),
    ("v2_future.rfdrec", 157, 0x2157_eb8f_2b6f_7cf8),
];

fn path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/formats/rfdrec")
        .join(name)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn dump(rec: &Recording) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "protocol_version: {}", rec.protocol_version);
    let _ = writeln!(out, "client_name: {}", rec.client_name);
    let _ = writeln!(out, "dcc: {}", rec.dcc);
    let _ = writeln!(out, "records: {}", rec.records.len());
    for line in rec.dump_lines(&Limits::default()) {
        let _ = writeln!(out, "{line}");
    }
    for (i, r) in rec.records.iter().enumerate() {
        let hex: String = r.frame.iter().map(|b| format!("{b:02x}")).collect();
        let _ = writeln!(out, "frame {i}: {hex}");
    }
    out
}

fn assert_golden(name: &str, actual: &str) {
    let golden = path(name);
    if std::env::var_os("REGIA_BLESS_FORMAT_FIXTURES").is_some_and(|v| v == "1") && !golden.exists() {
        std::fs::write(&golden, actual).unwrap();
    }
    let expected =
        std::fs::read_to_string(&golden).unwrap_or_else(|e| panic!("read {}: {e}", golden.display()));
    assert_eq!(
        actual, expected,
        "{name}: the reader's result differs from the golden dump"
    );
}

#[test]
fn fixtures_are_frozen() {
    for (name, len, hash) in FIXTURES {
        let bytes = std::fs::read(path(name)).unwrap();
        assert_eq!(
            (bytes.len(), fnv1a(&bytes)),
            (len, hash),
            "{name} was edited (len, fnv1a)"
        );
    }
    for entry in std::fs::read_dir(path("")).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            name.ends_with(".golden.txt") || FIXTURES.iter().any(|(n, _, _)| *n == name),
            "{name} unlisted"
        );
    }
}

#[test]
fn v1_loads_to_the_golden_and_round_trips_byte_for_byte() {
    let bytes = std::fs::read(path("v1.rfdrec")).unwrap();
    let rec = Recording::from_bytes(&bytes).expect("v1 loads");
    assert_golden("v1.golden.txt", &dump(&rec));
    assert_eq!(
        rec.to_bytes().unwrap(),
        bytes,
        "the writer reproduces the v1 bytes"
    );
    assert_eq!(Recording::read_file(&path("v1.rfdrec")).unwrap(), rec);

    let empty = Recording::read_file(&path("v1_empty.rfdrec")).expect("empty v1 loads");
    assert!(empty.records.is_empty() && empty.client_name.is_empty());
}

#[test]
fn a_newer_version_is_refused_with_both_versions_named() {
    let err = Recording::read_file(&path("v2_future.rfdrec")).expect_err("v2 refused");
    assert_eq!(err, RecordingError::TooNew(2));
    assert_eq!(err.code(), "replay_corrupt");
    let msg = err.to_string();
    assert!(
        msg.contains("format_version 2") && msg.contains("newer") && msg.contains("(1)"),
        "{msg}"
    );
}
