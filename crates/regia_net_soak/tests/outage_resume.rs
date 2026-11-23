// SPDX-License-Identifier: MPL-2.0
//! The outage rows of the gate plan, in process: `flappy` and `long-outage` at the gate's seed and
//! lengths (spec 120 § 10.2 and § 10.3, ADR-10093). Seeded, virtual time, no wall clock.
//!
//! An outage beyond the removal timeout does not end the game's connection: the server holds the
//! player dormant for its resume grace, the client's reconnect timeline (ADR-8352) presents the
//! resume ticket, and the first accept is the same `PeerId`. The client is told once that the
//! link is interrupted and once that it is back.

use regia_net_soak::profiles::{LONG_OUTAGE_AT_MS, LONG_OUTAGES_MS, Profile, flap_long_outage_ms};
use regia_net_soak::report::{self, Bar, OutageCheck, ScenarioResult};
use regia_net_soak::workload::{self, RunRecord, Scenario, ServerMode};

/// The gate plan's seed (`cargo xtask net-soak --plan gate`).
const GATE_SEED: u64 = 1_592_590_337;

fn run(profile: &str, clients: usize, seconds: u64, seed: u64) -> (RunRecord, ScenarioResult) {
    run_profile(
        Profile::named(profile).expect("known profile"),
        clients,
        seconds,
        seed,
    )
}

fn run_profile(mut p: Profile, clients: usize, seconds: u64, seed: u64) -> (RunRecord, ScenarioResult) {
    p.fit_to(seconds * 1000);
    let s = Scenario {
        profile: p,
        clients,
        mode: ServerMode::Dedicated,
        seed,
        duration_ms: seconds * 1000,
        wire: Default::default(),
    };
    let record = workload::run(&s).expect("run");
    let result = report::evaluate(&record, 0);
    (record, result)
}

fn bar<'a>(r: &'a ScenarioResult, id: &str) -> &'a Bar {
    r.bars
        .iter()
        .find(|b| b.id == id)
        .unwrap_or_else(|| panic!("{}: no bar {id}", r.label))
}

fn outages(r: &ScenarioResult) -> &[OutageCheck] {
    &r.measures.as_ref().expect("v2 measures").outages
}

/// Every involuntary rejoin of `client` came back as the peer it lost, and the client's record
/// shows that one peer id on both sides of each.
fn assert_same_peer_throughout(record: &RunRecord, r: &ScenarioResult, client: usize) {
    let m = r.measures.as_ref().expect("v2 measures");
    let resumes: Vec<_> = m.resumes.iter().filter(|x| x.client == client).collect();
    assert!(!resumes.is_empty(), "client {client} rejoined: {:?}", m.resumes);
    for x in resumes {
        assert_eq!(x.after_peer, Some(x.before_peer), "{x:?}");
    }
    let rejoins = record
        .reconnects
        .iter()
        .filter(|x| x.client == client && x.kind == "involuntary");
    for rc in rejoins {
        assert!(
            rc.connected_ms
                .is_some_and(|t| t >= rc.link_up_ms && t <= rc.link_up_ms + report::RECONNECT_GRACE_MS),
            "client {client} is back within the grace of its link's return: {rc:?}"
        );
    }
}

/// `long-outage` at the gate's client count, length and seed: 11 s, 25 s and 45 s two-way outages
/// at 60 s. The 25 s and 45 s outages run past the 20 s removal timeout; every client is told
/// once that the link is interrupted and once that it is back, and every one of them is the same
/// peer afterwards.
#[test]
fn long_outage_beyond_the_timeout_is_restored_once_as_the_same_peer() {
    let policy = report::timeout_policy();
    let (record, r) = run("long-outage", 8, 120, GATE_SEED);
    let all = outages(&r);
    assert_eq!(all.len(), 8, "{all:?}");
    for o in all {
        let len = LONG_OUTAGES_MS[o.client % 3];
        assert_eq!(
            (o.start_ms, o.end_ms),
            (LONG_OUTAGE_AT_MS, LONG_OUTAGE_AT_MS + len)
        );
        assert_eq!(
            (o.interrupted, o.restored, o.restore_owed),
            (1, 1, true),
            "one Interrupted and one Restored: {o:?}"
        );
        if len > policy.timeout_ms {
            assert_same_peer_throughout(&record, &r, o.client);
        } else {
            assert_eq!(record.clients[o.client].connected.len(), 1);
        }
    }
    let m = r.measures.as_ref().expect("v2 measures");
    assert_eq!(m.stray_interruptions, 0);
    // Client 5 also leaves on purpose at 36 s and is back long before the outage: one involuntary
    // rejoin per outage beyond the timeout, none other.
    let beyond = (0..8)
        .filter(|c| LONG_OUTAGES_MS[c % 3] > policy.timeout_ms)
        .count();
    assert_eq!(m.resumes.len(), beyond, "{:?}", m.resumes);
    // The server held each of them dormant and resumed it; nobody's hold ran out.
    let totals = &record.server.totals;
    assert_eq!(
        (totals.dormant, totals.dormant_expired),
        (beyond as u64, 0),
        "{totals:?}"
    );
    assert!(totals.resumes >= beyond as u64, "{totals:?}");
    for id in ["interrupted_once", "resume_same_peer"] {
        let b = bar(&r, id);
        assert!(b.passed && b.skipped.is_none(), "{id}: {}", b.detail);
    }
    // The harness bars are as before: both kinds of removal are the policy's own.
    for id in [
        "involuntary_disconnects",
        "reconnect_within_grace",
        "no_wedged_client",
    ] {
        let b = bar(&r, id);
        assert!(b.passed, "{id}: {}", b.detail);
    }
}

/// The same at three seeds and at the gate's larger client count: the resume does not depend on
/// one packet history.
#[test]
fn long_outage_resumes_every_client_at_other_seeds_and_32_clients() {
    for (clients, seed) in [(8, 11), (8, 22), (32, GATE_SEED)] {
        let (_, r) = run("long-outage", clients, 120, seed);
        for id in [
            "interrupted_once",
            "resume_same_peer",
            "involuntary_disconnects",
            "reconnect_within_grace",
        ] {
            let b = bar(&r, id);
            assert!(b.passed, "{clients} clients, seed {seed}: {id}: {}", b.detail);
        }
    }
}

/// `flappy` at the gate's client count, length and seed: a 3 s outage every 20 s, and on every
/// fourth client one outage 2 s longer than the removal timeout. The long outage ends with one
/// Restored, as the same peer.
#[test]
fn flappy_long_outage_is_restored_once_as_the_same_peer() {
    let (record, r) = run("flappy", 8, 60, GATE_SEED);
    let long = flap_long_outage_ms();
    let all = outages(&r);
    let longs: Vec<&OutageCheck> = all.iter().filter(|o| o.end_ms - o.start_ms == long).collect();
    assert_eq!(
        longs.iter().map(|o| o.client).collect::<Vec<_>>(),
        [3, 7],
        "{all:?}"
    );
    for o in &longs {
        assert_eq!(
            (o.interrupted, o.restored, o.restore_owed),
            (1, 1, true),
            "one Interrupted and one Restored: {o:?}"
        );
        assert_same_peer_throughout(&record, &r, o.client);
    }
    let i = bar(&r, "interrupted_once");
    assert!(i.passed && i.skipped.is_none(), "{}", i.detail);
    for id in [
        "involuntary_disconnects",
        "reconnect_within_grace",
        "no_wedged_client",
    ] {
        let b = bar(&r, id);
        assert!(b.passed, "{id}: {}", b.detail);
    }
}

/// `flappy` at 32 clients (the gate's other row) and at two more seeds.
#[test]
fn flappy_is_interrupted_once_at_other_seeds_and_32_clients() {
    for (clients, seed) in [(32, GATE_SEED), (8, 11), (8, 22)] {
        let (_, r) = run("flappy", clients, 60, seed);
        for id in [
            "interrupted_once",
            "involuntary_disconnects",
            "reconnect_within_grace",
        ] {
            let b = bar(&r, id);
            assert!(b.passed, "{clients} clients, seed {seed}: {id}: {}", b.detail);
        }
    }
}

/// Six more seeds of each gate row at 8 clients. The reconnect attempts' jitter and every packet's
/// fate differ per seed; the outcome does not: every outage has its one Interrupted and one
/// Restored, every involuntary rejoin is the peer it lost, back within the grace of its link's
/// return, and no server hold expires.
#[test]
fn the_outage_rows_hold_over_a_seed_sweep() {
    for seed in [1, 2, 3, 5, 8, 13] {
        for (profile, seconds) in [("flappy", 60), ("long-outage", 120)] {
            let (record, r) = run(profile, 8, seconds, seed);
            let at = format!("{profile}, seed {seed}");
            for id in [
                "interrupted_once",
                "involuntary_disconnects",
                "reconnect_within_grace",
                "no_wedged_client",
            ] {
                let b = bar(&r, id);
                assert!(b.passed && b.skipped.is_none(), "{at}: {id}: {}", b.detail);
            }
            if profile == "long-outage" {
                let b = bar(&r, "resume_same_peer");
                assert!(b.passed && b.skipped.is_none(), "{at}: {}", b.detail);
            }
            let m = r.measures.as_ref().expect("v2 measures");
            assert!(!m.resumes.is_empty(), "{at}: an outage beyond the timeout");
            for x in &m.resumes {
                assert_eq!(x.after_peer, Some(x.before_peer), "{at}: {x:?}");
            }
            assert_eq!(m.stray_interruptions, 0, "{at}");
            // A timeline that ran out would be a fresh join, a new peer, above; a hold that
            // expired is counted here.
            let totals = &record.server.totals;
            assert_eq!(totals.dormant_expired, 0, "{at}: {totals:?}");
            assert_eq!(totals.dormant, m.resumes.len() as u64, "{at}: {totals:?}");
        }
    }
}

/// `flappy` with its long outage cut to 12 s, inside the 20 s removal timeout: the connection
/// lasts through it. One Interrupted and one Restored from the engine on the connection that was
/// interrupted, no end on either side and nobody dormant: the resume takes no part in an outage
/// the timeout policy survives.
#[test]
fn flappy_twelve_second_outage_is_restored_on_the_connection_it_interrupted() {
    const OUTAGE_MS: u64 = 12_000;
    let policy = report::timeout_policy();
    assert!(OUTAGE_MS >= policy.interrupt_ms && OUTAGE_MS < policy.timeout_ms);
    let mut p = Profile::named("flappy").expect("flappy");
    p.long_outage_ms = Some(OUTAGE_MS);
    let (record, r) = run_profile(p, 8, 60, GATE_SEED);
    let all = outages(&r);
    let longs: Vec<&OutageCheck> = all
        .iter()
        .filter(|o| o.end_ms - o.start_ms == OUTAGE_MS)
        .collect();
    assert_eq!(
        longs.iter().map(|o| o.client).collect::<Vec<_>>(),
        [3, 7],
        "{all:?}"
    );
    for o in &longs {
        assert_eq!(
            (o.interrupted, o.restored, o.restore_owed),
            (1, 1, true),
            "one Interrupted and one Restored: {o:?}"
        );
        let c = &record.clients[o.client];
        assert_eq!(c.connected.len(), 1, "client {} stays connected", o.client);
        assert!(
            record.ends.iter().all(|e| e.client != o.client),
            "{:?}",
            record.ends
        );
    }
    let m = r.measures.as_ref().expect("v2 measures");
    assert!(m.resumes.is_empty(), "{:?}", m.resumes);
    assert_eq!(m.stray_interruptions, 0);
    let totals = &record.server.totals;
    assert_eq!((totals.dormant, totals.dormant_expired), (0, 0), "{totals:?}");
    for id in [
        "interrupted_once",
        "involuntary_disconnects",
        "reconnect_within_grace",
        "no_wedged_client",
    ] {
        let b = bar(&r, id);
        assert!(b.passed && b.skipped.is_none(), "{id}: {}", b.detail);
    }
}

/// A run with the reconnect timeline in it replays from its seed: the attempts' jitter is drawn
/// from the resume ticket, which the server mints from its seeded secret, so a gate row still
/// reproduces with `cargo xtask net-soak --seed`.
#[test]
fn a_run_with_the_resume_is_deterministic_from_its_seed() {
    let (mut a, _) = run("long-outage", 3, 120, 7);
    let (mut b, _) = run("long-outage", 3, 120, 7);
    let totals = &a.server.totals;
    assert_eq!(
        (totals.dormant, totals.resumes >= 2),
        (2, true),
        "two clients resumed: {totals:?}"
    );
    for r in [&mut a, &mut b] {
        r.clear_host_measurements();
    }
    assert_eq!(a, b);
}
