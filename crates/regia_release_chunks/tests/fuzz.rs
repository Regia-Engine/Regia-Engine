// SPDX-License-Identifier: MPL-2.0
//! `release-signature` v1 (the detached release-manifest signature sidecar, ADR-8687) under seeded
//! mutation: `canonical_signature_json` answers the writer's canonical bytes or an error, never
//! panics or hangs, and whatever it accepts re-encodes to the same bytes. The fixture is the
//! registry example `docs/formats/release-signature.example.json`.
//!
//! This is the registry's former `release.signature` target. `regia_release_chunks` is a pipeline
//! crate and `regia_agent`, which holds the fuzz-target registry, is a runtime crate that must not
//! link the pipeline layer (ADR-10356), so the registry test
//! (`crates/regia_agent/tests/fuzz_targets.rs`) names these tests for the format, as it does for
//! `regia_pt` and `regia_asset`. The bars are the registry's own or higher, with fixed seeds and no
//! clock: the registry ran 120 syntax mutations and 200 value mutations through its time-boxed
//! harness and 1000 mutations for the round-trip property; these run 2000, 1000 and 1000.

use regia_release_chunks::canonical_signature_json;
use regia_test_utils::fuzz::{
    FuzzRng, fuzz_text_loader, fuzz_text_values_loader, mutate_text, mutate_text_values,
};

const FIXTURE: &str = include_str!("../../../docs/formats/release-signature.example.json");

/// Syntax-corrupting mutations through the time-boxed harness (the registry ran 120).
const HARNESS_ITERATIONS: usize = 2000;
/// Well-formed value mutations through the time-boxed harness (the registry ran 200).
const VALUE_ITERATIONS: usize = 1000;
/// Mutations for the round-trip property, half syntax and half value (the registry's count).
const PROPERTY_ITERATIONS: u32 = 1000;

/// Parse, check the shape, and re-encode: the registry target's loader, unchanged.
fn load(text: &str) -> Result<String, String> {
    let bytes = canonical_signature_json(text.as_bytes()).map_err(|err| err.to_string())?;
    String::from_utf8(bytes).map_err(|err| format!("bug: the canonical signature is not UTF-8: {err}"))
}

#[test]
fn release_signature_never_panics() {
    load(FIXTURE).expect("the documented example loads");
    fuzz_text_loader(FIXTURE, 0x5EED_0000, HARNESS_ITERATIONS, load);
}

#[test]
fn release_signature_values_never_panics() {
    fuzz_text_values_loader(FIXTURE, 0x00A1_0000, VALUE_ITERATIONS, load);
}

/// The registry's round-trip property: every mutation the reader accepts re-encodes to the same
/// bytes, and every rejection names a reason.
#[test]
fn release_signature_round_trips_what_it_accepts() {
    let mut rng = FuzzRng::new(0xF0_2200);
    let mut accepted = 0u32;
    let mut rejected = 0u32;
    for i in 0..PROPERTY_ITERATIONS {
        // Even iterations corrupt the syntax; odd ones keep it well formed and corrupt the values,
        // so the input reaches the shape checks.
        let input = if i % 2 == 0 {
            mutate_text(FIXTURE, &mut rng)
        } else {
            mutate_text_values(FIXTURE, &mut rng)
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load(&input)));
        match outcome {
            Ok(Ok(canonical)) => {
                accepted += 1;
                let again = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load(&canonical)));
                match again {
                    Ok(Ok(reencoded)) => assert_eq!(
                        reencoded, canonical,
                        "mutation {i}: the canonical signature does not round trip"
                    ),
                    Ok(Err(error)) => {
                        panic!("mutation {i}: the re-encoded signature no longer loads: {error}")
                    }
                    Err(payload) => {
                        panic!("mutation {i}: the reader panicked on the re-encoding: {payload:?}")
                    }
                }
            }
            Ok(Err(error)) => {
                assert!(!error.is_empty(), "mutation {i}: an empty error");
                rejected += 1;
            }
            Err(payload) => {
                let message = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_string()))
                    .unwrap_or_else(|| "(non-string panic payload)".to_string());
                panic!("mutation {i}: the reader panicked: {message}");
            }
        }
    }
    assert!(
        accepted > 0,
        "no mutation loaded: the property was never exercised"
    );
    assert!(
        rejected > 0,
        "no mutation was rejected: the fixture is not being corrupted"
    );
}
