// SPDX-License-Identifier: MPL-2.0
//! `regia_net::desync` unit tests: the ring, `locate`, the injection hook and the probe's second
//! stage (spec 120 § 7.6, ADR-8211).

use super::*;
use crate::lockstep::SeatId;

struct Fixed {
    world: u64,
    rows: Vec<(&'static str, u64)>,
}

impl DesyncHash for Fixed {
    fn world_hash(&self, _tick: u64) -> u64 {
        self.world
    }
    fn subsystem_hashes(&self, _tick: u64, out: &mut Vec<(&'static str, u64)>) {
        out.clear();
        out.extend(self.rows.iter().copied());
    }
}

fn state(tick: u64) -> Fixed {
    Fixed {
        world: tick * 1000,
        rows: vec![("a", tick), ("b", tick * 2), ("c", tick * 3)],
    }
}

fn rows(names: &[&str], ticks: &[(u64, u64, &[u64])]) -> ProbeRows {
    ProbeRows {
        names: names.iter().map(|n| (*n).to_owned()).collect(),
        rows: ticks
            .iter()
            .map(|(tick, world, hashes)| SubsystemRow {
                tick: *tick,
                world: *world,
                hashes: hashes.to_vec(),
            })
            .collect(),
    }
}

#[test]
fn the_ring_keeps_the_newest_240_ticks_and_returns_the_world_hash() {
    let mut ring = SubsystemRing::new();
    for tick in 0..600 {
        assert_eq!(ring.record(tick, &state(tick)), tick * 1000);
    }
    assert_eq!(ring.len(), ROW_RING_TICKS);
    assert_eq!(ring.span(), Some((360, 599)));
    assert_eq!(ring.names(), ["a", "b", "c"]);
    assert!(ring.row(359).is_none());
    assert_eq!(
        ring.row(400),
        Some(&SubsystemRow {
            tick: 400,
            world: 400_000,
            hashes: vec![400, 800, 1200]
        })
    );
    let w = ring.window(590, 1_000);
    assert_eq!(w.rows.len(), 10);
    assert_eq!(w.names, ["a", "b", "c"]);
}

#[test]
fn recording_an_earlier_tick_replaces_the_rows_from_it_on() {
    let mut ring = SubsystemRing::new();
    for tick in 0..50 {
        ring.record(tick, &state(tick));
    }
    // A replay from tick 30 with a different state.
    for tick in 30..50 {
        ring.push(tick, 7, &[("a", 1), ("b", 2), ("c", 3)]);
    }
    assert_eq!(ring.len(), 50);
    assert_eq!(ring.row(29).map(|r| r.world), Some(29_000));
    assert!((30..50).all(|t| ring.row(t).map(|r| r.world) == Some(7)));
    // Rewinding drops the rows from the tick on.
    ring.rewind(45);
    assert_eq!(ring.span(), Some((0, 44)));
}

#[test]
fn new_subsystem_names_start_the_ring_again_and_extra_names_are_cut() {
    let mut ring = SubsystemRing::new();
    ring.record(0, &state(0));
    ring.push(1, 1, &[("a", 1), ("z", 2)]);
    assert_eq!(ring.names(), ["a", "z"]);
    assert_eq!(ring.len(), 1, "rows under the old names are gone");
    let many: Vec<(&'static str, u64)> = (0..70)
        .map(|i| (&*Box::leak(format!("s{i:02}").into_boxed_str()), 1))
        .collect();
    ring.push(2, 2, &many);
    assert_eq!(ring.names().len(), MAX_SUBSYSTEMS);
    assert_eq!(ring.row(2).map(|r| r.hashes.len()), Some(MAX_SUBSYSTEMS));
}

#[test]
fn a_full_ring_reuses_its_rows() {
    let mut ring = SubsystemRing::new();
    for tick in 0..(ROW_RING_TICKS as u64 + 10) {
        ring.record(tick, &state(tick));
    }
    let before: Vec<*const u64> = ring.rows.iter().map(|r| r.hashes.as_ptr()).collect();
    for tick in (ROW_RING_TICKS as u64 + 10)..(3 * ROW_RING_TICKS as u64) {
        ring.record(tick, &state(tick));
    }
    let mut after: Vec<*const u64> = ring.rows.iter().map(|r| r.hashes.as_ptr()).collect();
    let mut before = before;
    before.sort();
    after.sort();
    assert_eq!(before, after, "the same row buffers go round");
}

#[test]
fn locate_names_the_first_diverged_tick_its_subsystems_and_the_last_agreed_tick() {
    let names = ["units", "economy", "orders"];
    let reference = rows(
        &names,
        &[
            (10, 1, &[1, 2, 3]),
            (11, 2, &[1, 2, 3]),
            (12, 3, &[1, 2, 3]),
            (13, 4, &[1, 2, 3]),
        ],
    );
    let diverged = rows(
        &names,
        &[
            (10, 1, &[1, 2, 3]),
            (11, 2, &[1, 2, 3]),
            (12, 9, &[1, 5, 6]),
            (13, 9, &[9, 9, 9]),
        ],
    );
    let found = locate(&diverged, &reference);
    assert_eq!(found.first_diverged_tick, Some(12));
    assert_eq!(found.last_agreed_tick, Some(11));
    assert_eq!(found.subsystem.as_deref(), Some("economy"));
    assert_eq!(found.subsystems, ["economy", "orders"]);
    // Equal rows: nothing to name.
    let same = locate(&reference, &reference);
    assert_eq!(
        (same.first_diverged_tick, same.last_agreed_tick),
        (None, Some(13))
    );
}

#[test]
fn locate_compares_by_name_skips_ticks_one_side_lacks_and_counts_a_lone_name() {
    // The reference lists the names in another order and lacks tick 11.
    let reference = rows(&["b", "a"], &[(10, 1, &[2, 1]), (12, 3, &[2, 1])]);
    let diverged = rows(
        &["a", "b"],
        &[(10, 1, &[1, 2]), (11, 2, &[7, 7]), (12, 3, &[1, 2])],
    );
    let found = locate(&diverged, &reference);
    assert_eq!(found.first_diverged_tick, None, "tick 11 is not compared");
    assert_eq!(found.last_agreed_tick, Some(12));
    // A subsystem only one side has differs.
    let extra = rows(&["a", "b", "c"], &[(10, 1, &[1, 2, 0])]);
    let found = locate(&extra, &reference);
    assert_eq!(found.subsystems, ["c"]);
    let found = locate(&reference, &extra);
    assert_eq!(found.subsystems, ["c"]);
    // Only the world hash differs: no subsystem to name.
    let world_only = rows(&["a", "b"], &[(10, 99, &[1, 2])]);
    let found = locate(&world_only, &diverged);
    assert_eq!((found.first_diverged_tick, found.subsystem), (Some(10), None));
}

#[test]
fn the_inject_hook_flips_one_subsystem_at_one_tick() {
    let inject = DesyncInject::new("economy", 100);
    assert!(inject.hits(100, "economy"));
    assert!(!inject.hits(99, "economy") && !inject.hits(100, "units"));
    assert_ne!(inject.flip(100, "economy", 5), 5);
    assert_eq!(inject.flip(101, "economy", 5), 5);
    assert_eq!(inject.flip(100, "units", 5), 5);
    let zero = DesyncInject {
        mask: 0,
        ..inject.clone()
    };
    assert_ne!(zero.flip(100, "economy", 5), 5, "a zero mask still flips");
}

#[test]
fn the_second_stage_reports_each_target_once_when_the_reference_has_answered() {
    let reference = Voter::Seat(SeatId(0));
    let a = Voter::Seat(SeatId(1));
    let b = Voter::Seat(SeatId(2));
    let mut probe = SubsystemProbe::new(120, Some(90), reference);
    assert_eq!(probe.add_target(a, 100), Some((99, 100)));
    assert_eq!(probe.add_target(b, 105), Some((104, 105)));
    assert_eq!(probe.add_target(a, 100), None, "asked once");
    assert_eq!(probe.add_target(reference, 100), None, "never the reference");
    // A target's rows before the reference's: nothing yet.
    assert!(probe.offer(a, rows(&["x", "y"], &[(99, 1, &[1, 1]), (100, 2, &[1, 9])])));
    assert!(probe.reports().is_empty());
    // An unasked voter is dropped.
    assert!(!probe.offer(Voter::Seat(SeatId(9)), ProbeRows::default()));
    // The reference answers a's window: a is reported, b waits for its rows.
    assert!(probe.offer(
        reference,
        rows(&["x", "y"], &[(99, 1, &[1, 1]), (100, 3, &[1, 2])])
    ));
    let reports = probe.reports();
    assert_eq!(reports.len(), 1);
    assert_eq!(
        (
            reports[0].voter,
            reports[0].first_diverged_tick,
            reports[0].last_agreed_tick
        ),
        (a, Some(100), Some(99))
    );
    assert_eq!(reports[0].subsystem.as_deref(), Some("y"));
    assert!(probe.reports().is_empty(), "once");
    // b's rows; the reference answers its window; b's rows are cut to the window asked.
    assert!(probe.offer(
        b,
        rows(
            &["x", "y"],
            &[(50, 0, &[0, 0]), (104, 1, &[4, 4]), (105, 1, &[8, 4])]
        )
    ));
    assert!(probe.offer(
        reference,
        rows(&["x", "y"], &[(104, 1, &[4, 4]), (105, 1, &[5, 4])])
    ));
    let reports = probe.reports();
    assert_eq!(reports.len(), 1);
    assert_eq!(
        (
            reports[0].voter,
            reports[0].first_diverged_tick,
            reports[0].subsystem.as_deref()
        ),
        (b, Some(105), Some("x"))
    );
    // A reply after every window was answered is not taken.
    assert!(!probe.offer(reference, ProbeRows::default()));
}

#[test]
fn a_reference_without_the_rows_still_ends_the_probe_with_the_checkpoint_as_last_agreed() {
    let reference = Voter::Observer;
    let a = Voter::Seat(SeatId(3));
    let mut probe = SubsystemProbe::new(240, Some(210), reference);
    probe.add_target(a, 220);
    probe.offer(a, rows(&["x"], &[(219, 1, &[1]), (220, 2, &[2])]));
    probe.offer(reference, rows(&["x"], &[]));
    let reports = probe.reports();
    assert_eq!(reports.len(), 1);
    assert_eq!(
        (
            reports[0].first_diverged_tick,
            reports[0].last_agreed_tick,
            reports[0].subsystem.clone()
        ),
        (None, Some(210), None)
    );
}

#[test]
fn long_names_are_cut_at_a_character_boundary() {
    let name = "é".repeat(40); // 80 bytes
    let cut = clip_name(&name);
    assert!(cut.len() <= MAX_SUBSYSTEM_NAME_BYTES);
    assert_eq!(cut.chars().count(), 32);
    assert_eq!(clip_name("short"), "short");
}

#[test]
fn reason_codes_are_protocol_codes() {
    for code in [DESYNC, HOST_DESYNC, DESYNC_UNRESOLVED] {
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{code}"
        );
    }
}
