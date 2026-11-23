// SPDX-License-Identifier: MPL-2.0
//! The `fcap` target (ADR-2860): a level's local reflection capture bake. The loader reads the
//! file (magic, version, CRC, sizes, values) and writes it back; a mutated file must only be
//! refused by name.

use regia_asset::local_capture_bake::{FCAP_FORMAT_VERSION, FcapCapture, FcapFile, fcap_mip_bytes};

use super::{FuzzInput, FuzzTarget};

/// Two captures of a 16-wide chain (three mips), with distinct texels.
fn fixture() -> Vec<u8> {
    let capture = |id: u64, x: f32| FcapCapture {
        id,
        center: [x, 1.0, 0.0],
        half_extent: [2.0, 1.5, 2.0],
        weight: 1.0,
        fade: 0.5,
        coverage: 0.625,
        mips: (0..3)
            .map(|m| {
                (0..fcap_mip_bytes(16, m))
                    .map(|i| (i as u64 * 7 + id) as u8)
                    .collect()
            })
            .collect(),
    };
    FcapFile {
        width: 16,
        captures: vec![capture(1, 0.0), capture(2, 4.0)],
    }
    .to_bytes()
    .unwrap_or_default()
}

fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let file = FcapFile::from_bytes(bytes).map_err(|e| e.to_string())?;
    file.to_bytes().map_err(|e| e.to_string())
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![FuzzTarget {
        name: "fcap",
        format: Some("fcap"),
        format_version: FCAP_FORMAT_VERSION,
        round_trip: true,
        input: FuzzInput::Bytes { fixture, load },
    }]
}
