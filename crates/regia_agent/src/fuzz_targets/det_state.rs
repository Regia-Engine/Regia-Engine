// SPDX-License-Identifier: MPL-2.0
//! The lockstep state snapshot reader (`regia_detsim::state`, DC-3.7a): `det_state_snapshot`. The
//! fixture is a `SimHarness` snapshot of the engine's `disc_collide` scenario mid-run; the loader
//! checks the frame (magic, versions, kind, tag, length, integrity hash), resumes the harness and
//! saves it again, which is the canonical encoding. Snapshots travel inside desync bundles and
//! lockstep replays, so the bytes are untrusted.

use regia_detsim::golden::scenarios::CollideScenario;
use regia_detsim::golden::{DetRun, TickRate};
use regia_detsim::harness::SimHarness;
use regia_detsim::state::FRAME_VERSION;

use super::{FuzzInput, FuzzTarget};

/// A `disc_collide` run saved at tick 90 of 200.
fn fixture() -> Vec<u8> {
    let run = DetRun::new(3, 200, TickRate::DEFAULT);
    let Ok(mut harness) = SimHarness::new(CollideScenario::new(), run) else {
        return Vec::new();
    };
    if harness.step_n(90).is_err() {
        return Vec::new();
    }
    harness.save().unwrap_or_default()
}

/// Resume and re-save. Every refusal is an `Err` with the reader's code, never a panic.
fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let harness = SimHarness::<CollideScenario>::resume(bytes).map_err(|e| e.to_string())?;
    harness.save().map_err(|e| e.to_string())
}

/// The snapshot target.
pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![FuzzTarget {
        name: "det_state_snapshot",
        format: None,
        format_version: u32::from(FRAME_VERSION),
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
        for cut in 0..bytes.len() {
            assert!(load(&bytes[..cut]).is_err(), "a prefix of {cut} bytes loads");
        }
    }
}
