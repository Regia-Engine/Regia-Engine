// SPDX-License-Identifier: MPL-2.0
//! `memory::LeakCheck`, `MemorySnapshot` and `MemoryGrowth` at their extremes: zero warm-up, window of
//! one, huge bounds and values, and the untracked snapshot. No allocator is installed here, so only the
//! pure arithmetic is checked.

use regia_core::memory::{LeakCheck, MemoryGrowth, MemorySnapshot, Subsystem, SubsystemMemory};

#[test]
fn leak_check_without_warmup_and_with_a_window_of_one() {
    let mut check = LeakCheck::new(0, 1);
    check.sample(10);
    assert!(check.verdict(0).is_none(), "one window is not enough");
    check.sample(30);
    let v = check.verdict(100).expect("two windows of one cycle");
    assert_eq!((v.first_peak, v.last_peak, v.growth, v.cycles), (10, 30, 20, 2));
    assert!(v.bounded);
    check.sample(5);
    let v = check.verdict(0).expect("still judged");
    assert_eq!(
        (v.first_peak, v.last_peak, v.growth),
        (10, 5, -5),
        "shrinking is bounded"
    );
    assert!(v.bounded);
}

#[test]
fn leak_check_window_zero_is_treated_as_one() {
    let mut check = LeakCheck::new(2, 0);
    for value in [1, 1, 7, 9] {
        check.sample(value);
    }
    let v = check.verdict(0).expect("window 1 after a warm-up of 2");
    assert_eq!((v.first_peak, v.last_peak, v.growth), (7, 9, 2));
    assert!(!v.bounded);
}

#[test]
fn leak_check_growth_equal_to_the_bound_is_bounded() {
    let mut check = LeakCheck::new(0, 2);
    for value in [0, 4, 8, 6] {
        check.sample(value);
    }
    let v = check.verdict(4).expect("two windows");
    assert_eq!((v.first_peak, v.last_peak, v.growth), (4, 8, 4));
    assert!(v.bounded, "growth <= bound");
    assert!(!check.verdict(3).expect("same data").bounded);
}

#[test]
fn leak_check_ignores_the_warmup_peak_and_an_incomplete_trailing_window() {
    let mut check = LeakCheck::new(3, 2);
    for value in [1_000_000, 1_000_000, 1_000_000, 10, 20, 11, 12, 999_999] {
        check.sample(value);
    }
    let v = check.verdict(0).expect("two complete windows after warm-up");
    assert_eq!(
        (v.first_peak, v.last_peak),
        (20, 12),
        "the 999_999 sits in a window not yet complete"
    );
    assert_eq!(v.cycles, 8);
    assert_eq!(check.cycles(), 8);
}

#[test]
fn leak_check_survives_extreme_values_and_bounds() {
    // A bound above i64::MAX is a bound nobody can exceed; it must not wrap negative.
    let mut check = LeakCheck::new(0, 1);
    check.sample(0);
    check.sample(1_000);
    let v = check.verdict(u64::MAX).expect("two windows");
    assert!(v.bounded, "growth 1000 <= u64::MAX");
    // Peaks past i64::MAX must not overflow the subtraction.
    let mut big = LeakCheck::new(0, 1);
    big.sample(0);
    big.sample(u64::MAX);
    let v = big.verdict(0).expect("two windows");
    assert_eq!(v.growth, i64::MAX, "the growth saturates");
    assert!(!v.bounded);
    let mut shrink = LeakCheck::new(0, 1);
    shrink.sample(u64::MAX);
    shrink.sample(0);
    assert_eq!(shrink.verdict(0).expect("two windows").growth, i64::MIN);
    // A warm-up near u64::MAX must not overflow when the first window end is computed.
    let mut late = LeakCheck::new(u64::MAX, 5);
    late.sample(1);
    assert!(late.verdict(0).is_none());
}

#[test]
fn untracked_snapshot_lists_every_subsystem_and_says_it_is_untracked() {
    let snap = MemorySnapshot::default();
    assert!(!snap.tracking);
    assert_eq!(snap.live_bytes, 0);
    for (i, (subsystem, m)) in snap.subsystems.iter().enumerate() {
        assert_eq!(*subsystem, Subsystem::ALL[i]);
        assert_eq!(*m, SubsystemMemory::default());
        assert_eq!(snap.subsystem(*subsystem), *m);
    }
    assert!(snap.table().contains("not tracked"));
    let growth = snap.growth_since(&snap);
    assert_eq!(
        growth,
        MemoryGrowth {
            live_bytes: 0,
            subsystems: growth.subsystems
        }
    );
    assert!(growth.subsystems.iter().all(|(_, b, a)| *b == 0 && *a == 0));
    assert_eq!(growth.largest(), None);
}

#[test]
fn tracked_table_lists_only_subsystems_that_allocated() {
    let mut snap = MemorySnapshot {
        tracking: true,
        live_bytes: 3 << 20,
        peak_bytes: 5 << 20,
        ..Default::default()
    };
    snap.subsystems[Subsystem::Render as usize].1 = SubsystemMemory {
        live_bytes: 2 << 20,
        peak_bytes: 4 << 20,
        live_allocations: 12,
        total_allocations: 99,
        total_bytes: 1 << 30,
    };
    let table = snap.table();
    assert!(
        table.starts_with("CPU heap: 3.0 MiB live, 5.0 MiB peak\n"),
        "{table}"
    );
    assert!(table.contains("render") && table.contains("12 allocs"), "{table}");
    assert!(!table.contains("scene") && !table.contains("physics"), "{table}");
    assert_eq!(table.lines().count(), 2);
}

#[test]
fn growth_reports_negative_changes_and_ties_pick_a_deterministic_largest() {
    let mut earlier = MemorySnapshot::default();
    let mut later = MemorySnapshot::default();
    earlier.live_bytes = 1000;
    later.live_bytes = 400;
    earlier.subsystems[Subsystem::Scene as usize].1.live_bytes = 900;
    later.subsystems[Subsystem::Scene as usize].1.live_bytes = 100;
    later.subsystems[Subsystem::Ui as usize].1.live_bytes = 64;
    later.subsystems[Subsystem::Agent as usize].1.live_bytes = 64;
    later.subsystems[Subsystem::Agent as usize].1.live_allocations = 3;
    let g = later.growth_since(&earlier);
    assert_eq!(g.live_bytes, -600);
    assert_eq!(g.bytes(Subsystem::Scene), -800);
    let again = later.growth_since(&earlier);
    assert_eq!(g.largest(), again.largest(), "repeatable");
    let (winner, bytes) = g.largest().expect("two grew");
    assert_eq!(bytes, 64);
    assert!(matches!(winner, Subsystem::Ui | Subsystem::Agent));
    assert_eq!(g.subsystems[Subsystem::Agent as usize].2, 3);
}

#[test]
fn subsystem_display_and_names_agree() {
    for s in Subsystem::ALL {
        assert_eq!(s.to_string(), s.name());
    }
    assert_eq!(Subsystem::default(), Subsystem::Unscoped);
    assert_eq!(Subsystem::from_name(""), None);
    assert_eq!(Subsystem::from_name("Scene"), None, "names are case-sensitive");
}
