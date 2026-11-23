// SPDX-License-Identifier: MPL-2.0
//! Review of the `no_wedged_client` harness hole (ADR-8887, lane v07-net-soak-wedged-hole).
//!
//! These construct the clients the review was asked to try: one that stops receiving, one that
//! comes back after a leave and stays unheard, and a never-heard peer whose first input arrives
//! only after a short outage. They also pin that a gap the rule before ADR-8887 already reported
//! is still reported.

use regia_net_soak::profiles::Profile;
use regia_net_soak::report;
use regia_net_soak::workload::{self, INPUT_GRACE_MS, Knobs, Scenario, ServerMode, Silence, WEDGE_MS};

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

fn starved(client: usize, from_ms: u64, until_ms: u64) -> Knobs {
    Knobs {
        silent_snapshots: Some(Silence {
            client,
            from_ms,
            until_ms,
        }),
        ..Knobs::default()
    }
}

/// `Profile::client_link`'s per-client flap shift: `(index * 137) % 1_000`.
fn flap_shift(index: usize) -> u64 {
    (index as u64 * 137) % 1_000
}

fn bar<'a>(res: &'a report::ScenarioResult, id: &str) -> &'a report::Bar {
    res.bars
        .iter()
        .find(|b| b.id == id)
        .unwrap_or_else(|| panic!("bar {id}"))
}

/// The server withholds client 1's snapshots from 4 s on (the client keeps sending, the link stays
/// up). That is a client that stops receiving. `no_wedged_client` must fail with a `no_snapshot`
/// wedge that starts when the snapshots stop, not at the join.
#[test]
fn a_client_that_stops_receiving_is_wedged() {
    let s = short("clean", 4, 15, 3);
    let from = 4_000;
    let control = report::evaluate(&workload::run(&s).expect("control"), 0);
    let cb = bar(&control, "no_wedged_client");
    assert!(cb.passed && cb.gating, "control: {}", cb.detail);
    let res = report::evaluate(
        &workload::run_with(&s, &starved(1, from, u64::MAX)).expect("run"),
        0,
    );
    let b = bar(&res, "no_wedged_client");
    assert!(!b.passed && b.gating, "{}", b.detail);
    let snaps: Vec<_> = res
        .wedges
        .iter()
        .filter(|w| w.client == 1 && w.kind == "no_snapshot")
        .collect();
    assert!(!snaps.is_empty(), "{:?}", res.wedges);
    assert!(
        snaps[0].start_ms + 500 >= from && snaps[0].start_ms <= from + 500,
        "the wedge starts when snapshots stop, not at the join: {snaps:?}"
    );
    assert!(snaps.iter().all(|w| w.length_ms >= WEDGE_MS), "{snaps:?}");
}

/// Five seconds of no snapshots, then `flappy`'s first outage. The deaf stretch before the outage
/// is link-up time and must still be a wedge (an outage excuses itself and the grace after it,
/// not the silence that came before it).
#[test]
fn deaf_before_an_outage_is_still_wedged() {
    let s = short("flappy", 4, 40, 3);
    let (out_start, _) = s.profile.client_link(1, s.duration_ms).outages[0];
    let from = out_start - 5_000;
    let res = report::evaluate(
        &workload::run_with(&s, &starved(1, from, out_start + 1_000)).expect("run"),
        0,
    );
    let b = bar(&res, "no_wedged_client");
    assert!(!b.passed && b.gating, "{}", b.detail);
    let snaps: Vec<_> = res
        .wedges
        .iter()
        .filter(|w| w.client == 1 && w.kind == "no_snapshot")
        .collect();
    assert!(
        snaps
            .iter()
            .any(|w| w.start_ms + 500 >= from && w.start_ms < out_start && w.length_ms >= WEDGE_MS),
        "outage at {out_start}, silence from {from}: {snaps:?}"
    );
}

/// Client 5 leaves and rejoins (the workload's scripted reconnect) and sends no input on either
/// connection. Both connections are wedged, not only the one still open at the end of the run.
#[test]
fn a_client_that_rejoins_and_stays_silent_is_wedged_on_both_connections() {
    let s = short("clean", 8, 30, 3);
    let control = report::evaluate(&workload::run(&s).expect("control"), 0);
    assert!(
        bar(&control, "no_wedged_client").passed,
        "control: {}",
        bar(&control, "no_wedged_client").detail
    );
    let rec = workload::run_with(&s, &silent(5, 0, u64::MAX)).expect("run");
    let res = report::evaluate(&rec, 0);
    let connections = &rec.clients[5].connected;
    assert!(
        connections.len() >= 2,
        "the scripted leave must rejoin: {connections:?}"
    );
    let b = bar(&res, "no_wedged_client");
    assert!(!b.passed && b.gating, "{}", b.detail);
    let gaps: Vec<_> = res
        .wedges
        .iter()
        .filter(|w| w.client == 5 && w.kind == "server_no_input")
        .collect();
    assert_eq!(
        gaps.len(),
        connections.len(),
        "one wedge per connection\nconnections: {connections:?}\nwedges: {gaps:?}"
    );
    for (w, &(_, joined, left)) in gaps.iter().zip(connections.iter()) {
        assert!(
            w.start_ms >= joined && w.start_ms <= joined + INPUT_GRACE_MS + 100,
            "{w:?}, joined {joined}"
        );
        assert_eq!(w.start_ms + w.length_ms, left, "{w:?}, left {left}");
        assert!(w.length_ms >= WEDGE_MS, "{w:?}");
    }
}

/// Client 1 is dropped by a 22 s outage (the engine removes a started peer after 20 s of silence),
/// comes back for 8 s with the link up, and is dropped again. It sends no input on any connection.
/// Each link-up stretch after the join grace is several seconds, so every long connection is a
/// `server_no_input` wedge.
#[test]
fn a_client_that_loops_on_reconnect_without_input_is_wedged_each_time() {
    let mut s = short("clean", 4, 80, 3);
    let probe = workload::run(&s).expect("probe");
    let joined = probe.clients[1].connected[0].1;
    let shift = flap_shift(1);
    let link_up = 8_000;
    let out_len = 22_000;
    let period = out_len + link_up;
    let out_start = joined + link_up;
    s.profile.flaps = Some((out_start.saturating_sub(shift), period, out_len));
    let seen = s.profile.client_link(1, s.duration_ms).outages;
    assert!(seen.len() >= 2, "two outages so the client reconnects: {seen:?}");
    let control = report::evaluate(&workload::run(&s).expect("control"), 0);
    assert!(
        bar(&control, "no_wedged_client").passed,
        "control: {}",
        bar(&control, "no_wedged_client").detail
    );
    let rec = workload::run_with(&s, &silent(1, 0, u64::MAX)).expect("run");
    let connections = &rec.clients[1].connected;
    let long: Vec<_> = connections
        .iter()
        .copied()
        .filter(|&(_, j, left)| left.saturating_sub(j) >= INPUT_GRACE_MS + WEDGE_MS)
        .collect();
    assert!(
        long.len() >= 2,
        "the outages must drop and rejoin client 1 onto another long connection: {connections:?}"
    );
    let res = report::evaluate(&rec, 0);
    let b = bar(&res, "no_wedged_client");
    assert!(!b.passed && b.gating, "{}", b.detail);
    let gaps: Vec<_> = res
        .wedges
        .iter()
        .filter(|w| w.client == 1 && w.kind == "server_no_input")
        .collect();
    for &(_, joined_at, left) in &long {
        assert!(
            gaps.iter().any(|w| {
                w.start_ms >= joined_at
                    && w.start_ms <= joined_at + INPUT_GRACE_MS + 200
                    && w.start_ms + w.length_ms <= left
                    && w.length_ms >= WEDGE_MS
            }),
            "connection {joined_at}..{left}\nwedges: {gaps:?}"
        );
    }
}

/// A peer none of whose inputs have arrived, closed when the first one does, is judged from
/// [`INPUT_GRACE_MS`] after the join (`PeerState::open_gap`), not from the join itself.
///
/// Concrete input: client 1 joins, sends nothing, sits through a 400 ms outage that starts 1.5 s
/// after the grace, and delivers its first input as that outage's grace ends. Each link-up piece
/// after the join grace is 1.5 s, under [`WEDGE_MS`]. Counting the gap from the join (what the
/// arrival path records) makes the pre-outage piece the 1 s grace plus that 1.5 s, which is a
/// wedge. The bar must pass.
#[test]
fn a_first_input_after_one_short_outage_is_not_wedged_inside_the_grace() {
    let mut s = short("clean", 4, 20, 3);
    let probe = workload::run(&s).expect("probe");
    let joined = probe.clients[1].connected[0].1;
    let shift = flap_shift(1);
    let out_len = 400;
    let link_up_after_grace = 1_500;
    let out_start = joined + INPUT_GRACE_MS + link_up_after_grace;
    s.profile.flaps = Some((out_start.saturating_sub(shift), 100_000, out_len));
    let seen = s.profile.client_link(1, s.duration_ms).outages;
    assert_eq!(
        seen.first().copied(),
        Some((out_start, out_start + out_len)),
        "client 1 shift {shift}, flaps {:?}",
        s.profile.flaps
    );
    // The first input is sent as the excuse ends, so nothing after the outage is judged either.
    let until = out_start + out_len + INPUT_GRACE_MS;
    let control = report::evaluate(&workload::run(&s).expect("control"), 0);
    assert!(
        bar(&control, "no_wedged_client").passed,
        "control: {}",
        bar(&control, "no_wedged_client").detail
    );
    let rec = workload::run_with(&s, &silent(1, 0, until)).expect("run");
    assert_eq!(
        rec.clients[1].connected[0].1, joined,
        "the outage is after the join; the probe join time must hold"
    );
    let res = report::evaluate(&rec, 0);
    let b = bar(&res, "no_wedged_client");
    assert!(
        b.passed,
        "joined {joined}, outage {out_start}..{}, first input at {until}: neither judged piece \
         is {WEDGE_MS} ms once the join grace is excluded: {}",
        out_start + out_len,
        b.detail
    );
}

/// A server input gap the rule before ADR-8887 already reported (no outage, the client connected
/// as that peer for the whole gap, at least [`WEDGE_MS`] long) is still a wedge. The new judge
/// must not be easier to pass on that input.
#[test]
fn a_gap_the_old_rule_wedged_is_still_wedged() {
    let mut rec = workload::run(&short("clean", 1, 3, 1)).expect("run");
    rec.clients[0].joined = true;
    rec.clients[0].connected = vec![(7, 0, 10_000)];
    rec.links[0].outages.clear();
    rec.server.input_gaps = vec![(0, 7, 1_000, 5_000)];
    rec.client_gaps.clear();
    let w: Vec<_> = report::wedges(&rec)
        .into_iter()
        .filter(|w| w.kind == "server_no_input")
        .collect();
    assert_eq!(w.len(), 1, "{w:?}");
    assert_eq!((w[0].start_ms, w[0].length_ms), (1_000, 4_000));
}
