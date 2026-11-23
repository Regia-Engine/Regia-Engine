// SPDX-License-Identifier: MPL-2.0
//! The `.renv` target (ADR-2539): the product viewer's baked environment. The loader parses and validates
//! a file and re-encodes it.

use regia_viewer::{BakedEnv, ENV_FORMAT_VERSION};

use super::{FuzzInput, FuzzTarget};

/// The smallest environment a bake can hold: one mip of four texels per face.
fn fixture() -> Vec<u8> {
    BakedEnv::uniform([0.5, 0.25, 1.0]).to_bytes()
}

fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    BakedEnv::parse(bytes)
        .map(|env| env.to_bytes())
        .map_err(|e| e.to_string())
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![FuzzTarget {
        name: "viewer-env",
        format: Some("viewer-env"),
        format_version: ENV_FORMAT_VERSION,
        round_trip: true,
        input: FuzzInput::Bytes { fixture, load },
    }]
}
