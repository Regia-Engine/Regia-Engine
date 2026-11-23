// SPDX-License-Identifier: MPL-2.0
//! The per-poll scheduler (spec 120 § 5.3, ADR-8407) on seeded soak runs: a 1 MiB bulk transfer
//! to every client on `congested` leaves the clients' input arrival where a run without it has
//! it, and the state frames ("other unreliable") keep going while the bulk stream fills the rest
//! of the allowance.

use regia_net_soak::profiles::Profile;
use regia_net_soak::report::{self, Measures};
use regia_net_soak::workload::{self, Scenario, ServerMode};

/// The transfer of the acceptance row: 1 MiB to every client.
const BULK_1_MIB: usize = 1024 * 1024;

fn measures(bulk: Option<(u64, usize)>, seed: u64, seconds: u64) -> Measures {
    let mut profile = Profile::named("congested").expect("known profile");
    profile.fit_to(seconds * 1000);
    profile.bulk = bulk;
    let s = Scenario {
        profile,
        clients: 4,
        mode: ServerMode::Dedicated,
        seed,
        duration_ms: seconds * 1000,
        wire: Default::default(),
    };
    let r = report::evaluate(&workload::run(&s).expect("run"), 0);
    r.measures.expect("v2 measures")
}

fn missed(m: &Measures) -> f64 {
    m.input_ticks_missed as f64 / m.input_ticks_judged.max(1) as f64
}

/// Acceptance (spec 120 § 5.3): a 1 MiB bulk transfer plus inputs on `congested`: the input
/// arrival is unchanged against the same seeded run with no transfer (missed input within
/// +0.2 percentage points).
#[test]
fn a_1_mib_bulk_transfer_leaves_input_arrival_unchanged_on_congested() {
    let mut over = Vec::new();
    for seed in [6, 7, 8, 9] {
        let control = measures(None, seed, 60);
        let bulk = measures(Some((10_000, BULK_1_MIB)), seed, 60);
        assert!(control.input_ticks_judged > 1_000, "seed {seed}: {control:?}");
        let (c, b) = (missed(&control), missed(&bulk));
        let line = format!(
            "seed {seed}: missed input {:.2} % with the transfer, {:.2} % without; staleness p99 {:?} vs {:?}",
            100.0 * b,
            100.0 * c,
            bulk.staleness.iter().map(|g| g.p99_ticks).collect::<Vec<_>>(),
            control.staleness.iter().map(|g| g.p99_ticks).collect::<Vec<_>>()
        );
        println!("{line}");
        if b > c + 0.002 {
            over.push(line);
        }
    }
    assert!(over.is_empty(), "over the +0.2 point bar: {over:?}");
}

/// State frames are "other unreliable" and go before bulk (spec 120 § 5.3): with the transfer
/// running, the clients' snapshot staleness p99 stays within one tick of the run without it.
#[test]
fn state_frames_go_before_bulk_on_congested() {
    let seed = 6;
    let control = measures(None, seed, 60);
    let bulk = measures(Some((10_000, BULK_1_MIB)), seed, 60);
    let worst = |m: &Measures| m.staleness.iter().map(|g| g.p99_ticks).max().unwrap_or(0);
    assert!(
        worst(&bulk) <= worst(&control) + 1,
        "snapshot staleness p99 {} ticks with the transfer vs {} without",
        worst(&bulk),
        worst(&control)
    );
}
