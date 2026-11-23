// SPDX-License-Identifier: MPL-2.0
//! The § 2.4 create gate as fixed integer inputs (ADR-3943).
//!
//! No device, no clock. Frame-time milliseconds stay on [`regia_render::HitchReport`].

use regia_render::HitchFrame;
use regia_render::budget_ledger::{
    ALLOCATIONS, BIND_GROUP_CREATES, BudgetLedger, IDLE_FRAMES, PIPELINE_CREATES, PROBE_PREPARES,
};

#[test]
fn a_pipeline_create_after_the_warm_index_is_a_miss_and_zero_is_not() {
    let mut one = BudgetLedger::new();
    one.record(4, PIPELINE_CREATES, 1);
    assert_eq!(one.misses(4), vec![PIPELINE_CREATES.to_owned()]);
    assert_eq!(one.value(PIPELINE_CREATES, 4), 1);

    let mut zero = BudgetLedger::new();
    zero.record(4, PIPELINE_CREATES, 0);
    assert!(zero.misses(4).is_empty());
    assert_eq!(zero.value(PIPELINE_CREATES, 4), 0);
}

#[test]
fn a_pipeline_create_on_a_warm_frame_is_not_a_miss() {
    let mut ledger = BudgetLedger::new();
    ledger.record(0, PIPELINE_CREATES, 1);
    assert!(ledger.misses(1).is_empty());
    assert_eq!(ledger.value(PIPELINE_CREATES, 1), 0);
    assert_eq!(ledger.value(PIPELINE_CREATES, 0), 1);
}

#[test]
fn an_allocation_under_its_cap_is_not_a_miss() {
    let mut ledger = BudgetLedger::new();
    assert_eq!(ledger.cap(ALLOCATIONS), None);
    ledger.set_cap(ALLOCATIONS, 8);
    ledger.record(3, ALLOCATIONS, 7);
    let misses = ledger.misses(2);
    assert!(!misses.iter().any(|name| name == ALLOCATIONS), "{misses:?}");
    assert_eq!(ledger.value(ALLOCATIONS, 2), 7);
}

#[test]
fn an_allocation_over_its_cap_is_a_miss_and_equal_is_not() {
    let mut over = BudgetLedger::new();
    over.set_cap(ALLOCATIONS, 4);
    over.record(5, ALLOCATIONS, 5);
    assert_eq!(over.misses(5), vec![ALLOCATIONS.to_owned()]);

    let mut exact = BudgetLedger::new();
    exact.set_cap(ALLOCATIONS, 4);
    exact.record(5, ALLOCATIONS, 4);
    assert!(exact.misses(5).is_empty());
}

#[test]
fn an_uncapped_allocation_is_not_a_miss() {
    let mut ledger = BudgetLedger::new();
    ledger.record(0, ALLOCATIONS, 1_000_000);
    assert!(ledger.misses(0).is_empty());
}

#[test]
fn idle_frames_and_probe_prepares_start_uncapped() {
    let mut ledger = BudgetLedger::new();
    assert_eq!(ledger.cap(IDLE_FRAMES), None);
    assert_eq!(ledger.cap(PROBE_PREPARES), None);
    ledger.record(1, IDLE_FRAMES, 100);
    ledger.record(1, PROBE_PREPARES, 100);
    assert!(ledger.misses(0).is_empty());

    ledger.set_cap(IDLE_FRAMES, 10);
    ledger.set_cap(PROBE_PREPARES, 100);
    let misses = ledger.misses(0);
    assert_eq!(misses, vec![IDLE_FRAMES.to_owned()]);
}

#[test]
fn create_counters_start_at_cap_zero_and_misses_are_sorted() {
    let ledger = BudgetLedger::new();
    assert_eq!(ledger.cap(PIPELINE_CREATES), Some(0));
    assert_eq!(ledger.cap(BIND_GROUP_CREATES), Some(0));

    let mut both = BudgetLedger::new();
    both.record(0, PIPELINE_CREATES, 1);
    both.record(0, BIND_GROUP_CREATES, 2);
    assert_eq!(
        both.misses(0),
        vec![BIND_GROUP_CREATES.to_owned(), PIPELINE_CREATES.to_owned()]
    );
}

#[test]
fn a_later_negative_delta_can_clear_a_miss() {
    let mut ledger = BudgetLedger::new();
    ledger.record(1, PIPELINE_CREATES, 2);
    ledger.record(1, PIPELINE_CREATES, -2);
    assert!(ledger.misses(0).is_empty());
    assert_eq!(ledger.value(PIPELINE_CREATES, 0), 0);
}

#[test]
fn clearing_a_create_cap_stops_the_miss() {
    let mut ledger = BudgetLedger::new();
    ledger.record(0, PIPELINE_CREATES, 1);
    ledger.clear_cap(PIPELINE_CREATES);
    assert!(ledger.misses(0).is_empty());
    assert_eq!(ledger.cap(PIPELINE_CREATES), None);
}

#[test]
fn an_empty_name_is_ignored() {
    let mut ledger = BudgetLedger::new();
    ledger.record(0, "", 5);
    ledger.set_cap("", 0);
    assert!(ledger.misses(0).is_empty());
    assert_eq!(ledger.value("", 0), 0);
    assert_eq!(ledger.cap(""), None);
}

#[test]
fn hitch_frames_feed_the_create_counters() {
    let mut frames = vec![
        HitchFrame {
            frame_ms: 4.0,
            ..HitchFrame::default()
        };
        100
    ];
    frames[10].counters.bind_groups = 9;
    frames[11].counters.pipelines = 1;
    let mut ledger = BudgetLedger::new();
    ledger.record_hitch_frames(&frames);
    assert_eq!(ledger.value(BIND_GROUP_CREATES, 0), 9);
    assert_eq!(ledger.value(PIPELINE_CREATES, 0), 1);
    assert_eq!(
        ledger.misses(0),
        vec![BIND_GROUP_CREATES.to_owned(), PIPELINE_CREATES.to_owned()]
    );
    assert!(ledger.misses(12).is_empty());
}

#[test]
fn a_create_count_past_i64_saturates_and_misses() {
    let mut frame = HitchFrame::default();
    frame.counters.pipelines = u64::MAX;
    frame.counters.bind_groups = u64::MAX;
    let mut ledger = BudgetLedger::new();
    ledger.record_hitch_frame(0, &frame);
    assert_eq!(ledger.value(PIPELINE_CREATES, 0), i64::MAX);
    assert_eq!(ledger.value(BIND_GROUP_CREATES, 0), i64::MAX);
    assert_eq!(
        ledger.misses(0),
        vec![BIND_GROUP_CREATES.to_owned(), PIPELINE_CREATES.to_owned()]
    );
}

#[test]
fn a_long_frame_with_no_creates_is_not_a_ledger_miss() {
    let frames = vec![
        HitchFrame {
            frame_ms: 80.0,
            cpu_ms: 40.0,
            gpu_ms: 70.0,
            ..HitchFrame::default()
        };
        10
    ];
    let mut ledger = BudgetLedger::new();
    ledger.record_hitch_frames(&frames);
    assert!(ledger.misses(0).is_empty());
}

#[test]
fn deltas_add_within_and_across_measured_frames() {
    let mut ledger = BudgetLedger::new();
    ledger.set_cap(ALLOCATIONS, 8);
    ledger.record(2, ALLOCATIONS, 99);
    ledger.record(3, ALLOCATIONS, 1);
    ledger.record(3, ALLOCATIONS, 3);
    ledger.record(4, ALLOCATIONS, 5);
    assert_eq!(ledger.value(ALLOCATIONS, 3), 9);
    assert_eq!(ledger.misses(3), vec![ALLOCATIONS.to_owned()]);
    assert_eq!(ledger.value(ALLOCATIONS, 4), 5);
    assert!(ledger.misses(4).is_empty());
}

#[test]
fn a_caller_defined_name_starts_uncapped_and_can_be_capped() {
    let mut ledger = BudgetLedger::new();
    ledger.record(7, "staging_allocations", 3);
    assert_eq!(ledger.cap("staging_allocations"), None);
    assert!(ledger.misses(7).is_empty());
    ledger.set_cap("staging_allocations", 2);
    assert_eq!(ledger.misses(7), vec!["staging_allocations".to_owned()]);
}

#[test]
fn additions_saturate_within_and_across_frames() {
    let mut ledger = BudgetLedger::new();
    ledger.record(0, "positive", i64::MAX);
    ledger.record(0, "positive", 1);
    ledger.record(1, "positive", 1);
    ledger.record(0, "negative", i64::MIN);
    ledger.record(0, "negative", -1);
    ledger.record(1, "negative", -1);
    assert_eq!(ledger.value("positive", 0), i64::MAX);
    assert_eq!(ledger.value("negative", 0), i64::MIN);
}
