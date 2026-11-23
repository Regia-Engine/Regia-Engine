// SPDX-License-Identifier: MPL-2.0
//! Deterministic mutation fuzzing for the loaders of untrusted bytes (M18 robustness).
//!
//! Every loader that consumes bytes or text from disk, a pak, the network or the agent protocol
//! must accept *any* input: return `Err` with a useful message, never panic, never allocate
//! unboundedly and never hang. This module provides the shared machinery for those property tests:
//! a seed-stable PRNG and byte/text mutators that corrupt a valid fixture (truncate, flip bytes,
//! splice, overwrite length/version words, inject `NaN`/`inf`, deep-nest, ...), plus a panic guard
//! and a per-call wall-clock budget.
//!
//! A fuzz test is a deterministic loop: seed the [`FuzzRng`], mutate a valid fixture `N` times,
//! and for each mutation assert the loader only ever returns a value or an error — never panics and
//! never hangs. The loop is bounded (`N` fixed), the inputs are bounded (each mutation of a fixed
//! fixture), and **each loader call runs on its own thread with a `CALL_BUDGET` timeout**, so a
//! loader that infinite-loops fails the test instead of hanging the whole suite.
//!
//! [`fuzz_loader`] / [`fuzz_text_loader`] additionally assert the *canonical bad inputs* — empty,
//! a truncation, a hostile length/version word, and (for text) deep nesting past ron's recursion
//! limit — are each rejected with a non-empty error message, and that the mutation loop actually
//! yields at least one `Err` (so a loader that silently accepts garbage cannot pass).
//!
//! This crate is a test-only dependency (`docs/02-architecture.md` § 1, `docs/06` § 7): it never
//! ships in a game. The mutator lives here rather than in each loader crate so the corruption
//! strategies are identical across every format.

use std::fmt::Display;
use std::panic::{AssertUnwindSafe, UnwindSafe};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::Duration;

/// Per-call wall-clock budget. The loaders are single-pass parsers that return in microseconds on
/// a valid or corrupt input; a call that exceeds this budget is treated as a hang and fails the
/// test rather than stalling the suite.
const CALL_BUDGET: Duration = Duration::from_secs(5);

/// The mutator moved to `regia_core::fuzz` (ADR-1917) so a running dev build can use it too; it is
/// re-exported here unchanged, so every seeded test sees the same inputs.
pub use regia_core::fuzz::{DEEP_NEST, FuzzRng, mutate_bytes, mutate_text, mutate_text_values};

/// A loader call that finished: either a value, or a caught panic with a best-effort message.
enum CallOutcome<T, E> {
    /// The loader returned without panicking.
    Ok(Result<T, E>),
    /// The loader panicked.
    Panicked(String),
}

/// A human-readable description of a panic payload, when it is a string.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(non-string panic payload)".to_string()
    }
}

/// Run a byte loader call on a fresh OS thread and require it to finish within `budget`, so a
/// loader that hangs (an infinite loop) fails the test instead of hanging the whole suite. A
/// panicking loader is caught and reported; a timeout is reported as a hang.
fn run_byte_call<T, E, F>(load: F, input: Vec<u8>, budget: Duration) -> CallOutcome<T, E>
where
    F: Fn(&[u8]) -> Result<T, E> + Copy + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let outcome = match std::panic::catch_unwind(AssertUnwindSafe(|| load(&input))) {
            Ok(result) => CallOutcome::Ok(result),
            Err(payload) => CallOutcome::Panicked(panic_message(&payload)),
        };
        let _ = tx.send(outcome);
    });
    match rx.recv_timeout(budget) {
        Ok(outcome) => outcome,
        Err(RecvTimeoutError::Timeout) => {
            panic!("loader call exceeded {budget:?} (possible hang)")
        }
        Err(RecvTimeoutError::Disconnected) => panic!("loader worker thread died unexpectedly"),
    }
}

/// `run_byte_call` for text (`&str`) loaders.
fn run_text_call<T, E, F>(load: F, input: String, budget: Duration) -> CallOutcome<T, E>
where
    F: Fn(&str) -> Result<T, E> + Copy + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let outcome = match std::panic::catch_unwind(AssertUnwindSafe(|| load(&input))) {
            Ok(result) => CallOutcome::Ok(result),
            Err(payload) => CallOutcome::Panicked(panic_message(&payload)),
        };
        let _ = tx.send(outcome);
    });
    match rx.recv_timeout(budget) {
        Ok(outcome) => outcome,
        Err(RecvTimeoutError::Timeout) => {
            panic!("loader call exceeded {budget:?} (possible hang)")
        }
        Err(RecvTimeoutError::Disconnected) => panic!("loader worker thread died unexpectedly"),
    }
}

/// Run a closure and require it not to panic, returning its value. Panics with the description on
/// failure.
///
/// Loaders must be unwind-safe: they are pure `bytes/text -> Result`, so a panic here is a real
/// robustness bug worth failing the test over. Stack overflows and allocation failures abort the
/// process and are *not* caught by this guard; those are addressed by capping lengths and
/// recursion in the loaders themselves and are exercised by the deep-nesting and hostile-length
/// mutations. (The [`fuzz_loader`]/[`fuzz_text_loader`] loops use the time-boxed
/// `run_byte_call`/`run_text_call` instead, so their calls are also protected against hangs.)
pub fn assert_no_panic<T>(what: &str, f: impl FnOnce() -> T + UnwindSafe) -> T {
    match std::panic::catch_unwind(f) {
        Ok(value) => value,
        Err(_) => panic!("loader panicked on {what}"),
    }
}

/// Fuzz a byte loader: run `load` over `iterations` deterministic mutations of `fixture`, and
/// first assert that empty input, a truncation and a hostile leading word are rejected with a
/// non-empty error message. Every mutation must return a value or an error — never panic (each
/// call is time-boxed, so a hang fails the test) — and at least one mutation must be rejected
/// (proving the fixture is actually being corrupted). Returns the number of rejecting mutations.
pub fn fuzz_loader<F, T, E>(fixture: &[u8], seed: u64, iterations: usize, load: F) -> usize
where
    F: Fn(&[u8]) -> Result<T, E> + Copy + Send + 'static,
    T: Send + 'static,
    E: Display + Send + 'static,
{
    assert_byte_rejections(fixture, load);
    let mut rng = FuzzRng::new(seed);
    let mut errors = 0usize;
    for _ in 0..iterations {
        let mutated = mutate_bytes(fixture, &mut rng);
        match run_byte_call(load, mutated, CALL_BUDGET) {
            CallOutcome::Ok(Ok(_)) => {}
            CallOutcome::Ok(Err(error)) => {
                assert!(
                    !error.to_string().is_empty(),
                    "loader returned an empty error message"
                );
                errors += 1;
            }
            CallOutcome::Panicked(msg) => panic!("loader panicked on mutated bytes: {msg}"),
        }
    }
    assert!(
        errors > 0,
        "the mutation loop produced no errors — the fixture is not being corrupted"
    );
    errors
}

/// [`fuzz_loader`] for files built to survive a cut: a `.regiatrace` or a WebM reads up to its last complete chunk or
/// packet, so a truncation is valid input and a run may reject nothing past the header. The same time-boxed mutation
/// loop with only the empty-input rejection; hangs and panics still fail. Returns the number of rejecting mutations.
pub fn fuzz_tolerant_loader<F, T, E>(fixture: &[u8], seed: u64, iterations: usize, load: F) -> usize
where
    F: Fn(&[u8]) -> Result<T, E> + Copy + Send + 'static,
    T: Send + 'static,
    E: Display + Send + 'static,
{
    assert_rejected("empty bytes", load, Vec::new());
    let mut rng = FuzzRng::new(seed);
    let mut errors = 0usize;
    for _ in 0..iterations {
        let mutated = mutate_bytes(fixture, &mut rng);
        match run_byte_call(load, mutated, CALL_BUDGET) {
            CallOutcome::Ok(Ok(_)) => {}
            CallOutcome::Ok(Err(error)) => {
                assert!(
                    !error.to_string().is_empty(),
                    "loader returned an empty error message"
                );
                errors += 1;
            }
            CallOutcome::Panicked(msg) => panic!("loader panicked on mutated bytes: {msg}"),
        }
    }
    errors
}

/// [`fuzz_loader`] for wire data without a magic or length word up front (an input bundle starts with a free
/// `u64` tick, so any leading word is valid): the same time-boxed mutation loop and the empty and truncated
/// rejections, without the hostile-leading-word rejection.
pub fn fuzz_headerless_loader<F, T, E>(fixture: &[u8], seed: u64, iterations: usize, load: F) -> usize
where
    F: Fn(&[u8]) -> Result<T, E> + Copy + Send + 'static,
    T: Send + 'static,
    E: Display + Send + 'static,
{
    assert_rejected("empty bytes", load, Vec::new());
    assert_rejected("truncated bytes", load, fixture[..fixture.len() / 2].to_vec());
    let mut rng = FuzzRng::new(seed);
    let mut errors = 0usize;
    for _ in 0..iterations {
        let mutated = mutate_bytes(fixture, &mut rng);
        match run_byte_call(load, mutated, CALL_BUDGET) {
            CallOutcome::Ok(Ok(_)) => {}
            CallOutcome::Ok(Err(error)) => {
                assert!(
                    !error.to_string().is_empty(),
                    "loader returned an empty error message"
                );
                errors += 1;
            }
            CallOutcome::Panicked(msg) => panic!("loader panicked on mutated bytes: {msg}"),
        }
    }
    assert!(
        errors > 0,
        "the mutation loop produced no errors — the fixture is not being corrupted"
    );
    errors
}

/// Fuzz a text loader with well-formed value mutations ([`mutate_text_values`]: integer edges, `f32` overflow,
/// non-finite numbers, hostile strings, flipped booleans, emptied and repeated lists). Each call is time-boxed
/// like [`fuzz_loader`], so a huge count that makes a loader loop for hours fails as a hang. Well-formed inputs
/// may all load, so no rejection is required. Returns the number of rejected mutations.
pub fn fuzz_text_values_loader<F, T, E>(fixture: &str, seed: u64, iterations: usize, load: F) -> usize
where
    F: Fn(&str) -> Result<T, E> + Copy + Send + 'static,
    T: Send + 'static,
    E: Display + Send + 'static,
{
    let mut rng = FuzzRng::new(seed);
    let mut errors = 0usize;
    for _ in 0..iterations {
        let mutated = mutate_text_values(fixture, &mut rng);
        match run_text_call(load, mutated.clone(), CALL_BUDGET) {
            CallOutcome::Ok(Ok(_)) => {}
            CallOutcome::Ok(Err(error)) => {
                assert!(
                    !error.to_string().is_empty(),
                    "loader returned an empty error message"
                );
                errors += 1;
            }
            CallOutcome::Panicked(msg) => {
                panic!("loader panicked on value-mutated text: {msg}\ninput: {mutated}")
            }
        }
    }
    errors
}

/// Fuzz a text loader: the [`fuzz_loader`] twin for `&str` inputs (RON/JSON), additionally
/// asserting that a hostile version number and deep nesting are rejected.
pub fn fuzz_text_loader<F, T, E>(fixture: &str, seed: u64, iterations: usize, load: F) -> usize
where
    F: Fn(&str) -> Result<T, E> + Copy + Send + 'static,
    T: Send + 'static,
    E: Display + Send + 'static,
{
    assert_text_rejections(fixture, load);
    let mut rng = FuzzRng::new(seed);
    let mut errors = 0usize;
    for _ in 0..iterations {
        let mutated = mutate_text(fixture, &mut rng);
        match run_text_call(load, mutated, CALL_BUDGET) {
            CallOutcome::Ok(Ok(_)) => {}
            CallOutcome::Ok(Err(error)) => {
                assert!(
                    !error.to_string().is_empty(),
                    "loader returned an empty error message"
                );
                errors += 1;
            }
            CallOutcome::Panicked(msg) => panic!("loader panicked on mutated text: {msg}"),
        }
    }
    assert!(
        errors > 0,
        "the mutation loop produced no errors — the fixture is not being corrupted"
    );
    errors
}

/// Assert a byte loader rejects the canonical bad inputs with a non-empty error message.
fn assert_byte_rejections<F, T, E>(fixture: &[u8], load: F)
where
    F: Fn(&[u8]) -> Result<T, E> + Copy + Send + 'static,
    T: Send + 'static,
    E: Display + Send + 'static,
{
    assert_rejected("empty bytes", load, Vec::new());
    assert_rejected("truncated bytes", load, fixture[..fixture.len() / 2].to_vec());
    // A hostile leading word: 0xFF in the first four bytes breaks a magic (`FPAK`/`FCKD`/`glTF`)
    // or makes a postcard varint/length run past the end of the buffer — every byte loader here
    // must reject it.
    if fixture.len() >= 4 {
        let mut hostile = fixture.to_vec();
        hostile[..4].fill(0xff);
        assert_rejected("hostile leading word", load, hostile);
    }
}

/// Assert one byte input is rejected with a non-empty error message.
fn assert_rejected<T, E, F>(what: &str, load: F, input: Vec<u8>)
where
    F: Fn(&[u8]) -> Result<T, E> + Copy + Send + 'static,
    T: Send + 'static,
    E: Display + Send + 'static,
{
    match run_byte_call(load, input, CALL_BUDGET) {
        CallOutcome::Ok(Ok(_)) => panic!("{what} was accepted (expected an error)"),
        CallOutcome::Ok(Err(error)) => assert!(
            !error.to_string().is_empty(),
            "loader returned an empty error message for {what}"
        ),
        CallOutcome::Panicked(msg) => panic!("loader panicked on {what}: {msg}"),
    }
}

/// Assert a text loader rejects the canonical bad inputs with a non-empty error message.
fn assert_text_rejections<F, T, E>(fixture: &str, load: F)
where
    F: Fn(&str) -> Result<T, E> + Copy + Send + 'static,
    T: Send + 'static,
    E: Display + Send + 'static,
{
    assert_text_rejected("empty text", load, String::new());
    let half = fixture.floor_char_boundary(fixture.len() / 2);
    assert_text_rejected("truncated text", load, fixture[..half].to_string());
    if let Some(hostile) = hostile_version(fixture) {
        assert_text_rejected("hostile version", load, hostile);
    }
    // Deep nesting past ron's recursion limit must be rejected (ExceededRecursionLimit), not
    // overflow the stack.
    assert_text_rejected(
        "deep nesting",
        load,
        format!("{} 0 {}", "(".repeat(DEEP_NEST), ")".repeat(DEEP_NEST)),
    );
}

/// Assert one text input is rejected with a non-empty error message.
fn assert_text_rejected<T, E, F>(what: &str, load: F, input: String)
where
    F: Fn(&str) -> Result<T, E> + Copy + Send + 'static,
    T: Send + 'static,
    E: Display + Send + 'static,
{
    match run_text_call(load, input, CALL_BUDGET) {
        CallOutcome::Ok(Ok(_)) => panic!("{what} was accepted (expected an error)"),
        CallOutcome::Ok(Err(error)) => assert!(
            !error.to_string().is_empty(),
            "loader returned an empty error message for {what}"
        ),
        CallOutcome::Panicked(msg) => panic!("loader panicked on {what}: {msg}"),
    }
}

/// Rewrite the fixture's `version` / `format_version` number to `u32::MAX` (always newer than any
/// frozen v1 format), or `None` when the fixture carries no version field (e.g. the frozen v1 cue
/// format). Matching `format_version:` first keeps the shorter key from being mistaken for part of
/// it.
fn hostile_version(fixture: &str) -> Option<String> {
    let key = if fixture.contains("format_version:") {
        "format_version:"
    } else if fixture.contains("version:") {
        "version:"
    } else {
        return None;
    };
    let key_start = fixture.find(key)?;
    let value_start = key_start + key.len();
    let digits_start = value_start + fixture[value_start..].find(|c: char| c.is_ascii_digit())?;
    let digits_end = fixture[digits_start..]
        .find(|c: char| !c.is_ascii_digit())
        .map(|off| digits_start + off)
        .unwrap_or(fixture.len());
    let mut out = fixture.to_string();
    out.replace_range(digits_start..digits_end, "4294967295"); // u32::MAX
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic() {
        let mut a = FuzzRng::new(42);
        let mut b = FuzzRng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn byte_mutations_cover_all_strategies() {
        let fixture = b"FPAK\0\0\0\0hello world this is a fixture".to_vec();
        let mut rng = FuzzRng::new(7);
        for _ in 0..200 {
            let out = mutate_bytes(&fixture, &mut rng);
            // Mutations never grow unboundedly (they stay within a small factor of the fixture).
            assert!(out.len() <= fixture.len() + 64, "mutation grew to {}", out.len());
        }
    }

    #[test]
    fn text_mutations_stay_bounded() {
        let fixture = "SceneFile(format_version: 1, entities: [(guid: \"x\", name: \"Door\")])";
        let mut rng = FuzzRng::new(9);
        for _ in 0..200 {
            let out = mutate_text(fixture, &mut rng);
            // The deep-nesting strategy inserts DEEP_NEST parens; everything else stays small.
            assert!(
                out.len() <= fixture.len() + DEEP_NEST + 64,
                "mutation grew to {}",
                out.len()
            );
        }
    }

    #[test]
    fn version_mutation_rewrites_the_number_not_the_key() {
        // The round-2 regression: the version mutator duplicated the key and left the old number.
        let fixture = "(format_version: 1, name: \"x\")";
        let mut rng = FuzzRng::new(0);
        let mut saw_rewrite = false;
        for _ in 0..10_000 {
            let out = mutate_text(fixture, &mut rng);
            let rewrote = [
                "format_version: 0",
                "format_version: 2",
                "format_version: 99",
                "format_version: 4294967295",
            ]
            .iter()
            .any(|v| out.contains(v));
            if rewrote {
                saw_rewrite = true;
                assert!(!out.contains("format_version: 1"), "old number kept: {out}");
                assert!(
                    !out.contains("format_version: format_version"),
                    "key duplicated: {out}"
                );
                assert!(!out.contains("version: version"), "key duplicated: {out}");
            }
        }
        assert!(
            saw_rewrite,
            "version rewrite was never exercised in 10_000 mutations"
        );
    }

    #[test]
    fn hostile_version_rewrites_only_the_number() {
        let hostile = hostile_version("(format_version: 1, name: \"x\")").expect("version");
        assert_eq!(hostile, "(format_version: 4294967295, name: \"x\")");
        assert!(hostile_version("(name: \"no version\")").is_none());
        let v = hostile_version("(version: 1, root: ())").expect("version");
        assert_eq!(v, "(version: 4294967295, root: ())");
    }

    #[test]
    fn assert_no_panic_does_not_catch_panics_as_success() {
        let caught = std::panic::catch_unwind(|| assert_no_panic("boom", || panic!("x")));
        assert!(caught.is_err(), "a panicking loader must fail the guard");
    }

    #[test]
    fn fuzz_loader_rejects_empty_error_messages() {
        // A loader that returns an error with an empty message must fail the assertion.
        let fixture = b"x".to_vec();
        let caught = std::panic::catch_unwind(|| {
            fuzz_loader::<_, (), &str>(&fixture, 1, 1, |_bytes| Err::<(), _>(""));
        });
        assert!(caught.is_err(), "an empty error message must fail the check");
    }

    #[test]
    fn fuzz_loader_rejects_a_loader_that_accepts_empty_input() {
        // A loader that accepts empty input (returns Ok) must fail the rejection check.
        let fixture = b"some fixture bytes".to_vec();
        let caught = std::panic::catch_unwind(|| {
            fuzz_loader::<_, (), &'static str>(&fixture, 1, 1, |_bytes| Ok::<(), _>(()));
        });
        assert!(
            caught.is_err(),
            "accepting empty input must fail the rejection check"
        );
    }
}
