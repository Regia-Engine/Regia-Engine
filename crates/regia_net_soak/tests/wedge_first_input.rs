// SPDX-License-Identifier: MPL-2.0
//! The silence before a peer's first input (ADR-10107, on ADR-8424 and ADR-8887).
//!
//! The server judges a peer it has never heard from `INPUT_GRACE_MS` after its join, the moment
//! `missed_input` starts judging it. A removal and the run's end already closed that gap that way;
//! the arrival of the first input now does too. These pin that a long silence after the join is
//! still a wedge when one input finally arrives, that the gap starts at the same moment however it
//! is closed, and where the 2 s bar sits for a peer that has not been heard yet.

use regia_net_soak::profiles::Profile;
use regia_net_soak::report;
use regia_net_soak::workload::{
    self, INPUT_GRACE_MS, Knobs, RunRecord, Scenario, ServerMode, Silence, WEDGE_MS,
};

fn short(profile: &str, clients: usize, seconds: u64, seed: u64) -> Scenario {
    Scenario {
        profile: Profile::named(profile).expect("known profile"),
        clients,
        mode: ServerMode::Dedicated,
        seed,
        duration_ms: seconds * 1000,
        wire: Default::default(),
    }
}

fn silent(client: usize, from_ms: u64, until_ms: u64) -> Knobs {
    Knobs {
        silent_inputs: Some(Silence {
            client,
            from_ms,
            until_ms,
        }),
        ..Knobs::default()
    }
}

fn bar<'a>(res: &'a report::ScenarioResult, id: &str) -> &'a report::Bar {
    res.bars
        .iter()
        .find(|b| b.id == id)
        .unwrap_or_else(|| panic!("bar {id}"))
}

fn no_input(res: &report::ScenarioResult, client: usize) -> Vec<&workload::Wedge> {
    res.wedges
        .iter()
        .filter(|w| w.client == client && w.kind == "server_no_input")
        .collect()
}

/// Client 1's first connection start, from a run with no knobs (the silence does not move it).
fn first_join(s: &Scenario) -> u64 {
    workload::run(s).expect("probe").clients[1].connected[0].1
}

fn first_gap(rec: &RunRecord, client: usize) -> (u64, u64) {
    rec.server
        .input_gaps
        .iter()
        .find(|g| g.0 == client)
        .map(|g| (g.2, g.3))
        .unwrap_or_else(|| panic!("no input gap for client {client}: {:?}", rec.server.input_gaps))
}

/// Client 1 joins, sends nothing for 8 s, then sends as usual. The first input closes the gap, and
/// the stretch between the join grace and that input is a `server_no_input` wedge: a client silent
/// for a long time after its join is reported even though it is heard in the end.
#[test]
fn a_long_silence_after_the_join_then_one_input_is_wedged() {
    let s = short("clean", 4, 20, 3);
    let joined = first_join(&s);
    let until = 8_000;
    let control = report::evaluate(&workload::run(&s).expect("control"), 0);
    assert!(
        bar(&control, "no_wedged_client").passed,
        "control: {}",
        bar(&control, "no_wedged_client").detail
    );
    let rec = workload::run_with(&s, &silent(1, 0, until)).expect("run");
    assert_eq!(
        rec.clients[1].connected.len(),
        1,
        "{:?}",
        rec.clients[1].connected
    );
    let res = report::evaluate(&rec, 0);
    let b = bar(&res, "no_wedged_client");
    assert!(!b.passed && b.gating, "{}", b.detail);
    let w = no_input(&res, 1);
    assert_eq!(w.len(), 1, "{:?}", res.wedges);
    assert!(
        w[0].start_ms >= joined && w[0].start_ms <= joined + INPUT_GRACE_MS + 100,
        "the gap starts at the join grace: {w:?}, joined {joined}"
    );
    let end = w[0].start_ms + w[0].length_ms;
    assert!(
        end >= until && end <= until + 500,
        "the gap ends when the first input arrives: {w:?}, silent until {until}"
    );
    assert!(w[0].length_ms >= until - joined - INPUT_GRACE_MS - 100, "{w:?}");
}

/// The same never-heard silence closed three ways starts at the same moment: `INPUT_GRACE_MS`
/// after the server's join. Client 5 (the workload's scripted leave at 30 % of the run) is closed
/// by its first input or by its leave; client 1 by its first input or by the end of the run.
/// Before ADR-10107 the first input closed it from the join itself, 1 s earlier than the others.
#[test]
fn a_never_heard_gap_starts_at_the_join_grace_however_it_is_closed() {
    let s = short("clean", 8, 30, 1);
    let leave = s.duration_ms * 3 / 10;
    let by_input = workload::run_with(&s, &silent(5, 0, leave - 3_000)).expect("by input");
    let by_leave = workload::run_with(&s, &silent(5, 0, leave)).expect("by leave");
    let (a, a_end) = first_gap(&by_input, 5);
    let (b, b_end) = first_gap(&by_leave, 5);
    assert_eq!(a, b, "input-closed {a}..{a_end}, leave-closed {b}..{b_end}");
    assert!(a_end < b_end, "{a_end} {b_end}");
    let joined = by_input.clients[5].connected[0].1;
    assert!(
        a + 200 >= joined + INPUT_GRACE_MS && a <= joined + INPUT_GRACE_MS + 100,
        "{a}, joined {joined}"
    );
    let by_input = workload::run_with(&s, &silent(1, 0, 6_000)).expect("by input");
    let by_end = workload::run_with(&s, &silent(1, 0, u64::MAX)).expect("by end");
    let (c, c_end) = first_gap(&by_input, 1);
    let (d, d_end) = first_gap(&by_end, 1);
    assert_eq!(c, d, "input-closed {c}..{c_end}, end-closed {d}..{d_end}");
    assert!(c_end < d_end, "{c_end} {d_end}");
    let joined = by_input.clients[1].connected[0].1;
    assert!(
        c + 200 >= joined + INPUT_GRACE_MS && c <= joined + INPUT_GRACE_MS + 100,
        "{c}, joined {joined}"
    );
}

/// Where the bar sits for a peer not heard yet, with its link up the whole time and no outage:
/// the 2 s are counted from the end of the join grace. A first input 2.6 s after the grace is a
/// wedge; one 1.4 s after it is not (before ADR-10107 that one was, because the arrival counted
/// from the join while a leave or the run's end counted from the grace).
#[test]
fn the_wedge_bar_counts_from_the_join_grace_for_a_peer_not_heard_yet() {
    let s = short("clean", 4, 20, 3);
    let joined = first_join(&s);
    let late = joined + INPUT_GRACE_MS + WEDGE_MS + 600;
    let res = report::evaluate(&workload::run_with(&s, &silent(1, 0, late)).expect("late"), 0);
    let b = bar(&res, "no_wedged_client");
    assert!(
        !b.passed && b.gating,
        "first input at {late}, joined {joined}: {}",
        b.detail
    );
    assert_eq!(no_input(&res, 1).len(), 1, "{:?}", res.wedges);
    let soon = joined + INPUT_GRACE_MS + WEDGE_MS - 600;
    let rec = workload::run_with(&s, &silent(1, 0, soon)).expect("soon");
    assert!(
        rec.server.input_gaps.iter().all(|g| g.0 != 1),
        "1.4 s after the grace is no gap: {:?}, joined {joined}",
        rec.server.input_gaps
    );
    let res = report::evaluate(&rec, 0);
    let b = bar(&res, "no_wedged_client");
    assert!(b.passed, "first input at {soon}, joined {joined}: {}", b.detail);
}
