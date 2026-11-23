// SPDX-License-Identifier: MPL-2.0
//! The lockstep input log reader (`regia_detsim::replay`, DC-3.7b): `det_input_log`. The fixture is
//! the engine's committed `input_log_arena` log (the AI arena's start snapshot and 600 ticks of team
//! orders with recorded hashes); the loader checks the frame, decodes the log (seat order, hash
//! flags, the carried snapshot's header) and encodes it again, which is the canonical encoding. Input
//! logs travel inside desync bundles and lockstep replays, so the bytes are untrusted.

use regia_detsim::arena::ArenaOrder;
use regia_detsim::golden::scenarios::INPUT_LOG_ARENA_LOG;
use regia_detsim::replay::{INPUT_LOG_VERSION, InputLog};

use super::{FuzzInput, FuzzTarget};

/// The committed `input_log_arena` log.
fn fixture() -> Vec<u8> {
    INPUT_LOG_ARENA_LOG.to_vec()
}

/// Decode and re-encode. Every refusal is an `Err` with the reader's code, never a panic.
fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let log = InputLog::<ArenaOrder>::decode(bytes).map_err(|e| e.to_string())?;
    log.encode().map_err(|e| e.to_string())
}

/// The input log target.
pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![FuzzTarget {
        name: "det_input_log",
        format: None,
        format_version: INPUT_LOG_VERSION,
        round_trip: true,
        input: FuzzInput::Bytes { fixture, load },
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixture_loads_and_round_trips() {
        let bytes = fixture();
        assert!(!bytes.is_empty());
        let canonical = load(&bytes).expect("loads");
        assert_eq!(canonical, bytes, "the fixture is already canonical");
        for cut in (0..bytes.len()).step_by(7) {
            assert!(load(&bytes[..cut]).is_err(), "a prefix of {cut} bytes loads");
        }
    }
}
