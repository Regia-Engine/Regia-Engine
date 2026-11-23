// SPDX-License-Identifier: MPL-2.0
//! One input through one registry target: the verdict `regia_agent::fuzz_targets::run` gives a mutated input, without
//! the mutation (ADR-2723). Shared by source (`#[path]`) so the libFuzzer harness (`fuzz/fuzz_targets/registry.rs`), the
//! stable replay (`cargo xtask fuzz replay`) and the gate test (`crates/regia_agent/tests/fuzz_findings.rs`) cannot
//! disagree on what a finding is. This file must stay free of dependencies other than `regia_agent` and `std`.

use std::panic::{AssertUnwindSafe, catch_unwind};

use regia_agent::fuzz_targets::{
    BUG_PREFIX, FailureKind, FuzzInput, FuzzTarget, check_round_trip_bytes, check_round_trip_text,
};

/// What a target did with an input that is not a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The input loaded and, where the target has a writer, its re-encoding is stable.
    Accepted,
    /// The input was rejected with a non-empty error.
    Rejected,
}

/// Why an input is a finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// What went wrong, in `fuzz.run`'s words.
    pub kind: FailureKind,
    /// The panic or property message.
    pub message: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

/// A panic's message.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(non-string panic payload)".to_string()
    }
}

/// `load`'s answer to one input, sorted the way `fuzz_targets::run` sorts it; `round_trip` runs on an `Ok`.
fn judge<T>(
    outcome: std::thread::Result<Result<T, String>>,
    round_trip: impl FnOnce(&T) -> Result<(), String>,
) -> Result<Verdict, Finding> {
    let finding = |kind, message| Err(Finding { kind, message });
    match outcome {
        Ok(Ok(canonical)) => match catch_unwind(AssertUnwindSafe(|| round_trip(&canonical))) {
            Ok(Ok(())) => Ok(Verdict::Accepted),
            Ok(Err(message)) => finding(FailureKind::RoundTrip, message),
            Err(payload) => finding(
                FailureKind::RoundTrip,
                format!("panicked on the re-encoding: {}", panic_text(payload.as_ref())),
            ),
        },
        Ok(Err(e)) if e.is_empty() => finding(FailureKind::EmptyError, String::new()),
        Ok(Err(e)) if e.starts_with(BUG_PREFIX) => finding(FailureKind::Bug, e),
        Ok(Err(_)) => Ok(Verdict::Rejected),
        Err(payload) => finding(FailureKind::Panic, panic_text(payload.as_ref())),
    }
}

/// Runs `input` through `target` once. A text target reads the input lossily: libFuzzer's byte mutations break UTF-8
/// constantly, and every input should still reach the parser, so a stray byte becomes U+FFFD rather than a skipped run.
pub fn check_one(target: &FuzzTarget, input: &[u8]) -> Result<Verdict, Finding> {
    match target.input {
        FuzzInput::Text { load, .. } => {
            let text = String::from_utf8_lossy(input);
            judge(catch_unwind(AssertUnwindSafe(|| load(&text))), |canonical| {
                if target.round_trip {
                    check_round_trip_text(load, canonical)
                } else {
                    Ok(())
                }
            })
        }
        FuzzInput::Bytes { load, .. } => judge(catch_unwind(AssertUnwindSafe(|| load(input))), |canonical| {
            if target.round_trip {
                check_round_trip_bytes(load, canonical)
            } else {
                Ok(())
            }
        }),
    }
}
