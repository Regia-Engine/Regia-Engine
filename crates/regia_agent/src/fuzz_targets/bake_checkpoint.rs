// SPDX-License-Identifier: MPL-2.0
//! The bake checkpoint container (`.bakeck`, ADR-3095): what `regia bake-gi --checkpoint` and
//! `regia bake-lightmaps --checkpoint` write beside their output and read back to resume.

use regia_asset::bake_checkpoint::{
    BAKE_CHECKPOINT_FORMAT_VERSION, BakeCheckpoint, BakeCheckpointHeader, BakeCheckpointKind,
};

use super::{FuzzInput, FuzzTarget};

/// A probe bake halfway through its main trace, with a small payload.
fn fixture() -> Vec<u8> {
    BakeCheckpoint {
        header: BakeCheckpointHeader {
            kind: BakeCheckpointKind::GiCpu,
            stage: 1,
            input_hash: [0x5a; 32],
            units_total: 48,
            units_done: 20,
            batches_done: 5,
            work_seconds: 12.5,
        },
        payload: (0u8..64).collect(),
    }
    .to_bytes()
}

/// What a resuming bake reads (`BakeCheckpoint::from_bytes`: the magic, version, lengths, counts
/// and both hashes), re-encoded.
fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    BakeCheckpoint::from_bytes(bytes)
        .map(|c| c.to_bytes())
        .map_err(|e| e.to_string())
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![FuzzTarget {
        name: "bake-checkpoint",
        format: Some("bake-checkpoint"),
        format_version: BAKE_CHECKPOINT_FORMAT_VERSION,
        round_trip: true,
        input: FuzzInput::Bytes { fixture, load },
    }]
}
