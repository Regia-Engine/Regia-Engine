// SPDX-License-Identifier: MPL-2.0
//! `regia_test_utils::alloc_counter::quiet_tracing` (ADR-5718): after it, a disabled `tracing` span
//! builds no field, in every build; before it, a whole-workspace build (where `tracing` has its
//! `log` feature) builds the field for the `log` fallback and a one-crate build does not.
//!
//! Its own test binary, one test: the state it changes is process-wide and set once.

use std::cell::Cell;

/// Opens and closes a span whose field expression counts how often it is evaluated.
fn span_field_evaluations() -> u32 {
    let built = Cell::new(0);
    let _span = tracing::info_span!(
        "check_conditions",
        name = {
            built.set(built.get() + 1);
            "a system's name".to_string()
        }
    )
    .entered();
    built.get()
}

#[test]
fn a_disabled_span_builds_no_field_once_tracing_is_quiet() {
    assert!(
        !tracing::dispatcher::has_been_set(),
        "nothing in this binary sets a dispatcher before the call"
    );
    // No dispatcher was ever set: the span is disabled. With `tracing/log` the field is built for
    // the `log` fallback anyway (1); without the feature it is not (0).
    let before = span_field_evaluations();
    println!(
        "field evaluations of a disabled span before the call: {before} (1 = this build has tracing/log)"
    );
    assert!(before <= 1);

    regia_test_utils::alloc_counter::quiet_tracing();
    assert!(tracing::dispatcher::has_been_set());
    for _ in 0..3 {
        assert_eq!(span_field_evaluations(), 0, "a disabled span built its field");
    }
    // On another thread too: the state is the process's, not the calling thread's.
    let on_a_new_thread = std::thread::spawn(span_field_evaluations).join().expect("thread");
    assert_eq!(
        on_a_new_thread, 0,
        "a disabled span built its field on a new thread"
    );

    // Idempotent, and no global subscriber was installed: a host can still install its own.
    regia_test_utils::alloc_counter::quiet_tracing();
    assert!(
        tracing::subscriber::set_global_default(tracing::subscriber::NoSubscriber::default()).is_ok(),
        "the call must leave the global subscriber slot free"
    );
}
