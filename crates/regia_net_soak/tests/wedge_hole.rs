// SPDX-License-Identifier: MPL-2.0
//! `no_wedged_client` sees every client whose input does not reach the server (ADR-8887).
//!
//! ADR-8424 closed the end-of-run case (a peer none of whose inputs ever arrived, still connected
//! at the end). Two holes stayed open, and each test here failed on them:
//! - a peer the server removed (a scripted leave, a lost connection) while its input was missing
//!   was never judged: its gap was recorded only when the next input arrived or at the end of the
//!   run, and the peer was gone by then;
//! - any outage overlapping a gap excused the whole gap, so a client silent for the rest of the
//!   run after a 3 s outage read "no wedge"; the client-side snapshot and send gaps had the same
//!   excuse, so a client that heard nothing after an outage read "no wedge" too.
//!
//! The silence comes from [`Knobs::silent_inputs`] (the client stays connected and keeps sending
//! its commands and chat, but sends no input) and [`Knobs::silent_snapshots`] (the server sends
//! the client everything but snapshots).

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

fn bar<'a>(res: &'a report::ScenarioResult, id: &str) -> &'a report::Bar {
    res.bars
        .iter()
        .find(|b| b.id == id)
        .unwrap_or_else(|| panic!("bar {id}"))
}

/// The run of `s` with `knobs`, evaluated; the same run without them passes `no_wedged_client`
/// (the control: the silence is what the bar reports).
fn silent_run(s: &Scenario, knobs: &Knobs) -> (workload::RunRecord, report::ScenarioResult) {
    let control = report::evaluate(&workload::run(s).expect("run"), 0);
    let b = bar(&control, "no_wedged_client");
    assert!(b.passed && b.gating, "control: {}", b.detail);
    let rec = workload::run_with(s, knobs).expect("run");
    let res = report::evaluate(&rec, 0);
    (rec, res)
}

/// A client that never sends an input fails the bar, from the input grace after its join to the
/// end of the run (the ADR-8424 case, pinned here with a client that really sends nothing).
#[test]
fn a_client_that_never_sends_fails_no_wedged_client() {
    let s = short("clean", 4, 15, 1);
    let (rec, res) = silent_run(&s, &silent(1, 0, u64::MAX));
    let b = bar(&res, "no_wedged_client");
    assert!(!b.passed && b.gating, "{}", b.detail);
    assert!(b.known_blockers.is_empty(), "{b:?}");
    assert_eq!(res.wedges.len(), 1, "{:?}", res.wedges);
    let w = &res.wedges[0];
    assert_eq!((w.client, w.kind.as_str()), (1, "server_no_input"), "{w:?}");
    let joined = rec.clients[1].connected[0].1;
    assert!(
        w.start_ms >= joined && w.start_ms <= joined + INPUT_GRACE_MS + 100,
        "{w:?}, joined at {joined} ms"
    );
    assert_eq!(w.start_ms + w.length_ms, s.duration_ms, "{w:?}");
}

/// Client 5 leaves at 30 % of the run (the workload's scripted leave) and rejoins 2 s later. Here
/// it sends nothing before the leave and everything after: its first peer is gone before any of
/// its input could arrive, and the bar must still fail on the 8 s it was connected unheard.
/// Before ADR-8887: no wedge, the bar passed.
#[test]
fn a_client_never_heard_before_its_leave_is_wedged() {
    let s = short("clean", 8, 30, 1);
    let leave = s.duration_ms * 3 / 10;
    let (rec, res) = silent_run(&s, &silent(5, 0, leave));
    let b = bar(&res, "no_wedged_client");
    assert!(!b.passed && b.gating, "{}", b.detail);
    assert_eq!(res.wedges.len(), 1, "{:?}", res.wedges);
    let w = &res.wedges[0];
    assert_eq!((w.client, w.kind.as_str()), (5, "server_no_input"), "{w:?}");
    let (_, joined, left) = rec.clients[5].connected[0];
    assert!(
        w.start_ms >= joined && w.start_ms <= joined + INPUT_GRACE_MS + 100,
        "{w:?}, joined at {joined} ms"
    );
    // The gap runs to the client's own leave: the server's removal comes a link delay later.
    assert_eq!(w.start_ms + w.length_ms, left, "{w:?}, left at {left} ms");
    assert!(w.length_ms >= leave - joined - INPUT_GRACE_MS - 100, "{w:?}");
    // After the rejoin it is heard again.
    assert!(
        rec.clients[5].connected.len() >= 2,
        "{:?}",
        rec.clients[5].connected
    );
}

/// The same client heard at first, then silent from 3 s to its leave at 9 s: the server's gap
/// starts at its last input and the peer is removed before another one arrives.
/// Before ADR-8887: no wedge, the bar passed.
#[test]
fn a_client_that_stops_sending_before_its_leave_is_wedged() {
    let s = short("clean", 8, 30, 1);
    let leave = s.duration_ms * 3 / 10;
    let quiet_from = 3_000;
    let (rec, res) = silent_run(&s, &silent(5, quiet_from, leave));
    let b = bar(&res, "no_wedged_client");
    assert!(!b.passed && b.gating, "{}", b.detail);
    assert_eq!(res.wedges.len(), 1, "{:?}", res.wedges);
    let w = &res.wedges[0];
    assert_eq!((w.client, w.kind.as_str()), (5, "server_no_input"), "{w:?}");
    // The last input left at most a frame before the silence and arrived a link delay later.
    assert!(
        w.start_ms >= quiet_from - 100 && w.start_ms <= quiet_from + 100,
        "{w:?}"
    );
    let left = rec.clients[5].connected[0].2;
    assert_eq!(w.start_ms + w.length_ms, left, "{w:?}, left at {left} ms");
}

/// `flappy`: a 3 s outage every 20 s (shorter than the engine's removal timeout, so the peer
/// stays). Client 1 goes silent at its first outage and stays silent 6 s past its end. The outage
/// and the input grace after it are excused; the 5 s after them are a wedge.
/// Before ADR-8887: any outage overlapping the gap excused all of it, so no wedge, the bar passed.
#[test]
fn silence_after_an_outage_is_wedged() {
    let s = short("flappy", 4, 40, 1);
    let link = s.profile.client_link(1, s.duration_ms);
    let (out_start, out_end) = link.outages[0];
    let quiet_until = out_end + 6_000;
    let (_, res) = silent_run(&s, &silent(1, out_start, quiet_until));
    let b = bar(&res, "no_wedged_client");
    assert!(!b.passed && b.gating, "{}", b.detail);
    assert_eq!(res.wedges.len(), 1, "{:?}", res.wedges);
    let w = &res.wedges[0];
    assert_eq!((w.client, w.kind.as_str()), (1, "server_no_input"), "{w:?}");
    // Judged from the input grace after the outage to the first input after the silence.
    assert_eq!(w.start_ms, out_end + INPUT_GRACE_MS, "{w:?}");
    let end = w.start_ms + w.length_ms;
    assert!(end >= quiet_until && end <= quiet_until + 300, "{w:?}");
    assert!(w.length_ms >= WEDGE_MS, "{w:?}");
}

/// The same silence ended inside the excuse (the outage plus the input grace) is no wedge: the
/// outage still excuses what it covers.
#[test]
fn silence_inside_an_outage_and_its_grace_is_no_wedge() {
    let s = short("flappy", 4, 40, 1);
    let link = s.profile.client_link(1, s.duration_ms);
    let (out_start, out_end) = link.outages[0];
    let rec = workload::run_with(&s, &silent(1, out_start, out_end + INPUT_GRACE_MS / 2)).expect("run");
    let res = report::evaluate(&rec, 0);
    let b = bar(&res, "no_wedged_client");
    assert!(b.passed, "{}", b.detail);
}

/// The downlink twin: the server withholds client 1's snapshots from its first `flappy` outage to
/// 6 s past its end. The outage and the grace after it are excused; the rest is a `no_snapshot`
/// wedge. Before ADR-8887: the client-side gap overlapped the outage, so no wedge, the bar passed.
#[test]
fn no_snapshot_after_an_outage_is_wedged() {
    let s = short("flappy", 4, 40, 1);
    let link = s.profile.client_link(1, s.duration_ms);
    let (out_start, out_end) = link.outages[0];
    let quiet_until = out_end + 6_000;
    let (_, res) = silent_run(&s, &starved(1, out_start, quiet_until));
    let b = bar(&res, "no_wedged_client");
    assert!(!b.passed && b.gating, "{}", b.detail);
    let snap: Vec<_> = res.wedges.iter().filter(|w| w.kind == "no_snapshot").collect();
    assert!(!snap.is_empty(), "{:?}", res.wedges);
    assert!(res.wedges.iter().all(|w| w.client == 1), "{:?}", res.wedges);
    // The first judged gap starts where the excuse ends; the gaps cover the silence after it.
    assert_eq!(snap[0].start_ms, out_end + INPUT_GRACE_MS, "{:?}", snap);
    let covered: u64 = snap.iter().map(|w| w.length_ms).sum();
    assert!(
        covered >= quiet_until - out_end - INPUT_GRACE_MS - WEDGE_MS,
        "{snap:?}"
    );
}

/// A client the server never sends a snapshot is wedged while connected (the control: the
/// same run with snapshots passes).
#[test]
fn a_client_never_sent_a_snapshot_is_wedged() {
    let s = short("clean", 4, 15, 1);
    let (_, res) = silent_run(&s, &starved(2, 0, u64::MAX));
    let b = bar(&res, "no_wedged_client");
    assert!(!b.passed && b.gating, "{}", b.detail);
    assert!(
        res.wedges
            .iter()
            .any(|w| w.client == 2 && w.kind == "no_snapshot"),
        "{:?}",
        res.wedges
    );
    assert!(res.wedges.iter().all(|w| w.client == 2), "{:?}", res.wedges);
}

/// `report::judged_input_gap` on hand-set intervals: the gap is clipped to the client's own
/// connections as that peer, and each outage plus [`INPUT_GRACE_MS`] after it is cut out.
#[test]
fn judged_input_gap_clips_to_the_connection_and_cuts_out_outages() {
    let mut rec = workload::run(&short("clean", 1, 3, 1)).expect("run");
    let g = INPUT_GRACE_MS;
    // Connected as peer 7 from 1 s to 30 s, as peer 8 from 32 s to 60 s.
    rec.clients[0].connected = vec![(7, 1_000, 30_000), (8, 32_000, 60_000)];
    rec.links[0].outages = vec![(20_000, 23_000), (10_000, 11_000)];
    // No outage inside: the gap whole.
    assert_eq!(
        report::judged_input_gap(&rec, 0, 7, 2_000, 9_000),
        vec![(2_000, 9_000)]
    );
    // Ends after the client's own end: clipped to it.
    assert_eq!(
        report::judged_input_gap(&rec, 0, 7, 25_000, 31_000),
        vec![(25_000, 30_000)]
    );
    // Two outages (unsorted on the link): three judged parts.
    assert_eq!(
        report::judged_input_gap(&rec, 0, 7, 5_000, 29_000),
        vec![(5_000, 10_000), (11_000 + g, 20_000), (23_000 + g, 29_000)]
    );
    // A gap that starts after an outage ended is judged whole: the grace excuses only a gap the
    // outage overlaps (it never loosens what the rule before ADR-8887 judged).
    assert_eq!(
        report::judged_input_gap(&rec, 0, 7, 11_000 + g / 2, 15_000),
        vec![(11_000 + g / 2, 15_000)]
    );
    // Inside one outage and its grace: nothing.
    assert!(report::judged_input_gap(&rec, 0, 7, 20_500, 23_000 + g).is_empty());
    // Another peer's interval does not count, and an unknown client has none.
    assert!(report::judged_input_gap(&rec, 0, 9, 2_000, 9_000).is_empty());
    assert_eq!(
        report::judged_input_gap(&rec, 0, 8, 31_000, 40_000),
        vec![(32_000, 40_000)]
    );
    assert!(report::judged_input_gap(&rec, 5, 7, 2_000, 9_000).is_empty());
}
