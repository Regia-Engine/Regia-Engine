// SPDX-License-Identifier: MPL-2.0
//! `*.siminput.ron` compatibility against frozen fixtures (`tests/fixtures/formats/siminput/`):
//! the version 1 file loads to a golden dump, and a newer version is refused with both versions
//! named. Fixtures are frozen by length and FNV-1a hash.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use regia_net::session::hub::{SIM_RECORDING_FORMAT_VERSION, SimRecording};

const FIXTURES: [(&str, usize, u64); 2] = [
    ("v1.siminput.ron", 102, 0x2d4d_edc1_ffd0_5b7d),
    ("v2_future.siminput.ron", 102, 0x285a_c52b_6b59_859e),
];

fn path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/formats/siminput")
        .join(name)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
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
fn v1_loads_to_the_golden() {
    assert_eq!(SIM_RECORDING_FORMAT_VERSION, 1);
    let text = std::fs::read_to_string(path("v1.siminput.ron")).unwrap();
    let rec = SimRecording::from_ron(&text).expect("v1 loads");
    let mut dump = format!(
        "format_version: {}\nframes: {}\n",
        rec.format_version,
        rec.frames.len()
    );
    for (i, f) in rec.frames.iter().enumerate() {
        dump.push_str(&format!("frame {i}: {f:?}\n"));
    }
    let golden = path("v1.golden.txt");
    if std::env::var_os("REGIA_BLESS_FORMAT_FIXTURES").is_some_and(|v| v == "1") && !golden.exists() {
        std::fs::write(&golden, &dump).unwrap();
    }
    let expected = std::fs::read_to_string(&golden).unwrap();
    assert_eq!(dump, expected, "the reader's result differs from the golden dump");
}

#[test]
fn a_newer_version_is_refused_with_both_versions_named() {
    let text = std::fs::read_to_string(path("v2_future.siminput.ron")).unwrap();
    let err = SimRecording::from_ron(&text).expect_err("v2 refused");
    assert!(
        err.contains("format_version 2") && err.contains("is not 1"),
        "{err}"
    );
}
